//! Background preview loader. Every previewable file in the current folder is queued as soon as
//! the folder opens: images, videos, SVGs, the first page of PDFs and the covers of e-books and comics. Decoded pictures are uploaded as textures on the UI thread.
//!
//! Finished previews are also kept on disk (see `cache`), so reopening a folder, in this window or in
//! the next file dialog, never runs ffmpeg or a decoder again. Previews come in a few resolutions
//! that follow the icon size: zooming in re-renders at the next tier, once, and keeps the old
//! picture on screen until the sharper one arrives.
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use eframe::egui;

/// Preview resolutions, by the longest side in pixels.
const TIERS: [u32; 3] = [128, 256, 512];
/// Document pages and video frames need more pixels than a small tile to stay readable, so they
/// never render below this.
const PAGE_MIN: u32 = 256;
/// Decoding threads for a machine with `cores` cores: all but one, so the window keeps a core to itself,
/// and between two and six, since more would only hold more pictures in memory at once.
fn workers_for(cores: usize) -> usize {
    cores.saturating_sub(1).clamp(2, 6)
}

fn workers() -> usize {
    std::thread::available_parallelism().map_or(2, |cores| workers_for(cores.get()))
}
/// Textures uploaded per frame; the rest wait for the next one so a big folder never stalls a frame.
const UPLOADS_PER_FRAME: usize = 6;
/// Previews kept in memory, in bytes of pixels. Past this, opening a folder lets go of the other
/// folders' pictures; the disk cache brings them back quickly if they are needed again.
const TEXTURE_BUDGET: usize = 256 * 1024 * 1024;
/// PDFs and archives larger than this are not worth rendering for a tiny preview.
/// ffmpeg gets this long on one frame before it is killed.
const VIDEO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_DOC_BYTES: u64 = 64 * 1024 * 1024;
/// A PDF is read whole to find its first page, and every worker may do so at once.
const MAX_PDF_BYTES: u64 = 64 * 1024 * 1024;
/// A PDF that has not produced its first page by now is given up on.
const PDF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How many PDF renders may run on past their time (they cannot be stopped) before PDFs get no preview.
const MAX_RUNAWAY_PDFS: usize = 4;
/// What a helper process (ffmpeg, 7z) may print: one picture. More is a bomb, and is cut off.
const MAX_HELPER_OUTPUT: u64 = 64 * 1024 * 1024;
/// Pictures with more pixels than this (100 megapixels) get no preview: decoding one takes
/// gigabytes, and a file header can claim any size.
const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// What a file looked like when its preview was asked for: modification time and size. A preview
/// made for another stamp is out of date.
pub type Stamp = (Option<std::time::SystemTime>, u64);

fn stamp_of(path: &Path) -> Stamp {
    std::fs::metadata(path).map_or((None, 0), |m| (m.modified().ok(), m.len()))
}

struct Job {
    path: PathBuf,
    stamp: Stamp,
    generation: u64,
    tier: u32,
}

/// Jobs waiting for a worker. Workers take a job for something on screen before any other, so the
/// pictures being looked at appear first and the rest of a big folder fills in behind them.
#[derive(Default)]
struct Queue {
    jobs: Mutex<Waiting>,
    wake: std::sync::Condvar,
    /// The files on screen, top to bottom.
    visible: Mutex<Vec<PathBuf>>,
}

#[derive(Default)]
struct Waiting {
    jobs: std::collections::VecDeque<Job>,
    /// How many waiting jobs each file has, so a worker asks the few files on screen whether they
    /// wait instead of hashing every job of a big folder on each turn.
    count: HashMap<PathBuf, usize>,
    /// The queue was closed: nothing new is taken, and the workers leave.
    closed: bool,
}

impl Queue {
    fn push(&self, job: Job) {
        if let Ok(mut waiting) = self.jobs.lock() {
            if waiting.closed {
                return;
            }
            *waiting.count.entry(job.path.clone()).or_default() += 1;
            waiting.jobs.push_back(job);
        }
        self.wake.notify_one();
    }

    /// Ends the workers' wait for jobs: the jobs still waiting are dropped, a job a worker is
    /// busy with is finished, and every worker leaves the queue after that.
    fn close(&self) {
        let mut waiting = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        waiting.closed = true;
        waiting.jobs.clear();
        waiting.count.clear();
        drop(waiting);
        self.wake.notify_all();
    }

    /// The next job, waiting for one; `None` once the queue is closed.
    fn pop(&self) -> Option<Job> {
        let mut waiting = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if waiting.closed {
                return None;
            }
            if !waiting.jobs.is_empty() {
                let at = {
                    let visible = self.visible.lock().unwrap_or_else(|e| e.into_inner());
                    visible
                        .iter()
                        .find(|p| waiting.count.contains_key(*p))
                        .and_then(|p| waiting.jobs.iter().position(|j| &j.path == p))
                        .unwrap_or(0)
                };
                let job = waiting.jobs.remove(at).expect("index in range");
                if let Some(n) = waiting.count.get_mut(&job.path) {
                    *n -= 1;
                    if *n == 0 {
                        waiting.count.remove(&job.path);
                    }
                }
                return Some(job);
            }
            waiting = self.wake.wait(waiting).unwrap_or_else(|e| e.into_inner());
        }
    }
}

enum Outcome {
    Ready(egui::ColorImage, u32),
    Cancelled,
    Failed,
}

pub struct Thumbnails {
    /// Each texture with the tier it was rendered at.
    textures: HashMap<PathBuf, (egui::TextureHandle, u32)>,
    /// The stamp each texture was made for, and the newest stamp seen for each file: a result for any other is dropped.
    made: HashMap<PathBuf, Stamp>,
    wanted: HashMap<PathBuf, Stamp>,
    queued: HashMap<PathBuf, Stamp>,
    /// Resolution wanted for new and upgraded previews.
    tier: u32,
    /// Files of the open folder, and extra pictures such as folder icons, for re-queuing at a new tier.
    current: Vec<PathBuf>,
    extras: HashSet<PathBuf>,
    /// Files that have no preview, with the stamp they failed at; tried again once the file changes.
    failed: HashMap<PathBuf, Stamp>,
    queue: Arc<Queue>,
    result_rx: mpsc::Receiver<(PathBuf, Stamp, Outcome)>,
    generation: Arc<AtomicU64>,
}

impl Drop for Thumbnails {
    /// The workers wait for jobs for ever otherwise, and every window that was opened would leave
    /// its workers behind in a process that keeps running.
    fn drop(&mut self) {
        self.queue.close();
    }
}

/// Smallest tier that is at least `px` physical pixels, or the largest one.
fn tier_for(px: f32) -> u32 {
    TIERS.into_iter().find(|&t| t as f32 >= px).unwrap_or(TIERS[TIERS.len() - 1])
}

fn extension(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

pub fn is_image(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("png" | "apng" | "jpg" | "jpeg" | "jpe" | "jfif" | "gif" | "webp" | "bmp" | "ico" | "tif" | "tiff" | "tga" | "ppm" | "pgm" | "pbm" | "pnm" | "qoi" | "hdr" | "dds")
    )
}

/// Pictures the image decoder cannot read but ffmpeg can.
fn is_ffmpeg_still(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("heic" | "heif" | "avif" | "jxl" | "psd" | "exr" | "jp2" | "j2k" | "pcx"))
}

/// Camera RAW files, which carry a full-size JPEG next to the sensor data.
fn is_raw(path: &Path) -> bool {
    matches!(
        extension(path).as_deref(),
        Some("cr2" | "cr3" | "crw" | "nef" | "nrw" | "arw" | "sr2" | "srf" | "dng" | "orf" | "rw2" | "raf" | "pef" | "srw" | "3fr" | "erf" | "kdc" | "dcr" | "mrw" | "x3f" | "raw")
    )
}

/// Music files show their embedded cover art, when they have one.
fn is_audio(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("mp3" | "flac" | "m4a" | "m4b" | "ogg" | "oga" | "opus" | "wma" | "aac" | "ape" | "wv" | "mka" | "aif" | "aiff" | "mpc" | "dsf" | "tta"))
}

/// Comic archives, which are a folder of page pictures in a zip, rar or 7z.
fn is_comic(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("cbz" | "cbr" | "cb7" | "cbt"))
}

fn is_fb2(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
    name.ends_with(".fb2") || name.ends_with(".fb2.zip")
}

fn is_mobi(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("mobi" | "azw" | "azw3" | "prc"))
}

fn is_video(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" | "ogv" | "3gp" | "3g2" | "m2ts" | "m2t" | "mts" | "vob" | "asf" | "rm" | "rmvb" | "divx" | "f4v" | "mxf" | "ogm" | "mpe" | "qt" | "dv" | "ts"))
}

/// `.ts` is also TypeScript, so a transport stream is told apart by its 0x47 sync bytes, found
/// every 188 bytes (every 192 in a `.m2ts`, after a 4-byte timestamp).
fn is_transport_stream(path: &Path) -> bool {
    let mut head = [0u8; 193];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && ((head[0] == 0x47 && head[188] == 0x47) || (head[4] == 0x47 && head[192] == 0x47))
}

fn is_svg(path: &Path) -> bool {
    extension(path).as_deref() == Some("svg")
}

fn is_page(path: &Path) -> bool {
    matches!(
        extension(path).as_deref(),
        Some("pdf" | "epub" | "kra" | "ora")
    ) || is_comic(path)
        || is_fb2(path)
        || is_mobi(path)
}

/// Whether the file gets a picture of its contents instead of a type icon.
pub fn has_preview(path: &Path) -> bool {
    is_image(path) || is_ffmpeg_still(path) || is_raw(path) || is_video(path) || is_audio(path) || is_svg(path) || is_page(path)
}

impl Thumbnails {
    pub fn new(ctx: &egui::Context) -> Self {
        let queue = Arc::new(Queue::default());
        let (result_tx, result_rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));

        for _ in 0..workers() {
            let queue = queue.clone();
            let result_tx = result_tx.clone();
            let generation = generation.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                // Decoding is background work: it yields to the UI thread instead of competing with it
                unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::syscall(libc::SYS_gettid) as libc::id_t, 15) };
                while let Some(job) = queue.pop() {
                // Skip work for folders the user already left
                if job.generation != generation.load(Ordering::Relaxed) {
                    let _ = result_tx.send((job.path, job.stamp, Outcome::Cancelled));
                    continue;
                }
                // A malformed document must not take the worker down with it
                let decoded = std::panic::catch_unwind(|| cached_thumbnail(&job.path, job.tier)).ok().flatten();
                let outcome = decoded.map_or(Outcome::Failed, |img| Outcome::Ready(to_color_image(&img), job.tier));
                let _ = result_tx.send((job.path, job.stamp, outcome));
                ctx.request_repaint();
                }
            });
        }

        cache::trim_in_background();

        Self {
            textures: HashMap::new(),
            made: HashMap::new(),
            wanted: HashMap::new(),
            queued: HashMap::new(),
            tier: TIERS[0],
            current: Vec::new(),
            extras: HashSet::new(),
            failed: HashMap::new(),
            queue,
            result_rx,
            generation,
        }
    }

    fn enqueue(&mut self, path: &Path, stamp: Stamp, generation: u64) {
        self.wanted.insert(path.to_path_buf(), stamp);
        let sharp_enough = self.textures.get(path).is_some_and(|(_, tier)| *tier >= self.tier) && self.made.get(path) == Some(&stamp);
        let pending = self.queued.get(path) == Some(&stamp);
        if has_preview(path) && !sharp_enough && !pending && self.failed.get(path) != Some(&stamp) {
            self.queued.insert(path.to_path_buf(), stamp);
            self.queue.push(Job { path: path.to_path_buf(), stamp, generation, tier: self.tier });
        }
    }

    /// Tell the loader which files are on screen now; their previews are made first.
    pub fn set_visible<'a>(&self, paths: impl Iterator<Item = &'a Path>) {
        let paths: Vec<&Path> = paths.collect();
        if let Ok(mut visible) = self.queue.visible.lock() {
            // Called every frame: only rebuild the list (and its allocations) when the view moved
            if visible.len() == paths.len() && visible.iter().zip(&paths).all(|(a, b)| a == b) {
                return;
            }
            visible.clear();
            visible.extend(paths.into_iter().map(Path::to_path_buf));
        }
    }

    /// Queue every previewable file of a freshly opened folder, each with its stamp so that a picture
    /// changed since its preview was made is drawn again. Cancels work queued for the previous folder.
    pub fn preload<'a>(&mut self, files: impl Iterator<Item = (&'a Path, Stamp)>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        // Anything that was queued but not finished was dropped by the workers
        self.queued.clear();
        self.current.clear();
        self.extras.clear();
        // Files on servers are read only when the setting asks for that
        for (path, stamp) in files.filter(|(p, _)| crate::fs::network::previews_allowed(p)) {
            self.enqueue(path, stamp, generation);
            self.current.push(path.to_path_buf());
        }
        self.trim_textures();
    }

    /// Lets go of the pictures of other folders once they take more memory than `TEXTURE_BUDGET`,
    /// so browsing folder after folder of photos does not hold every preview ever shown.
    fn trim_textures(&mut self) {
        let total: usize = self.textures.values().map(|(tex, _)| tex.size().iter().product::<usize>() * 4).sum();
        if total <= TEXTURE_BUDGET {
            return;
        }
        let keep: HashSet<&Path> = self.current.iter().map(PathBuf::as_path).collect();
        self.textures.retain(|path, _| keep.contains(path.as_path()));
        self.made.retain(|path, _| keep.contains(path.as_path()));
    }

    /// Queue one extra image (e.g. a custom folder icon) without cancelling the folder's own work.
    /// Also the way to say a file changed: its preview is drawn again, the old one staying until then.
    pub fn ensure(&mut self, path: &Path) {
        let generation = self.generation.load(Ordering::Relaxed);
        self.extras.insert(path.to_path_buf());
        self.enqueue(path, stamp_of(path), generation);
    }

    /// Tell the loader how many physical pixels the largest preview is drawn at. Crossing into a
    /// bigger tier re-renders what is already loaded; going smaller keeps the sharper pictures.
    pub fn want_pixels(&mut self, px: f32) {
        let tier = tier_for(px);
        if tier <= self.tier {
            self.tier = tier;
            return;
        }
        self.tier = tier;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.queued.clear();
        let paths: Vec<PathBuf> = self.current.iter().chain(self.extras.iter()).cloned().collect();
        for path in paths {
            let stamp = self.wanted.get(&path).copied().unwrap_or_else(|| stamp_of(&path));
            self.enqueue(&path, stamp, generation);
        }
    }

    /// Upload finished thumbnails; call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let mut uploaded = 0;
        while uploaded < UPLOADS_PER_FRAME {
            let Ok((path, stamp, outcome)) = self.result_rx.try_recv() else { return };
            // The file changed after this job started: its result is for a picture that is gone
            let current = self.wanted.get(&path) == Some(&stamp);
            match outcome {
                Outcome::Ready(img, tier) => {
                    if !current {
                        continue;
                    }
                    if tier >= self.tier {
                        self.queued.remove(&path);
                    }
                    // A slow low-resolution job can finish after its upgrade did
                    if self.made.get(&path) == Some(&stamp) && self.textures.get(&path).is_some_and(|(_, have)| *have >= tier) {
                        continue;
                    }
                    let tex = ctx.load_texture(path.to_string_lossy(), img, egui::TextureOptions::LINEAR);
                    self.made.insert(path.clone(), stamp);
                    self.textures.insert(path, (tex, tier));
                    uploaded += 1;
                }
                Outcome::Cancelled => {
                    if self.queued.get(&path) == Some(&stamp) {
                        self.queued.remove(&path);
                    }
                }
                Outcome::Failed => {
                    if current {
                        self.queued.remove(&path);
                        self.failed.insert(path, stamp);
                    }
                }
            }
        }
        // More finished previews are waiting: draw this frame first, then take the next batch
        ctx.request_repaint();
    }

    pub fn get(&self, path: &Path) -> Option<&egui::TextureHandle> {
        self.textures.get(path).map(|(tex, _)| tex)
    }
}

fn to_color_image(img: &image::DynamicImage) -> egui::ColorImage {
    let rgba = img.to_rgba8();
    egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw())
}

/// The preview from the disk cache, or rendered now and cached for next time. Files that have no
/// preview are remembered too, so a folder of covers-less tracks does not spawn ffmpeg per file on
/// every visit.
fn cached_thumbnail(path: &Path, tier: u32) -> Option<image::DynamicImage> {
    let shared = is_image(path) && !is_ffmpeg_still(path);
    if shared && let Some(hit) = shared_cache::read(path, tier) {
        return Some(hit);
    }
    let Some(key) = cache::key(path) else { return render_thumbnail(path, tier) };
    if let Some(hit) = cache::read(&key, tier) {
        return Some(hit);
    }
    if cache::known_missing(&key) {
        return None;
    }
    let rendered = render_thumbnail(path, tier);
    match &rendered {
        Some(img) => {
            cache::write(&key, tier, img);
            if shared {
                shared_cache::write(path, tier, img);
            }
        }
        None => cache::mark_missing(&key),
    }
    rendered
}

/// A preview with its longest side at most `tier` pixels, or `PAGE_MIN` where the shape is kept.
fn render_thumbnail(path: &Path, tier: u32) -> Option<image::DynamicImage> {
    let page_size = tier.max(PAGE_MIN);
    if is_svg(path) {
        return render_svg(path, tier);
    }
    if is_page(path) {
        let page = if extension(path).as_deref() == Some("pdf") {
            if std::fs::metadata(path).ok()?.len() > MAX_PDF_BYTES {
                return None;
            }
            render_pdf(path, page_size)?
        } else {
            book_or_document_cover(path)?
        };
        return Some(page.thumbnail(page_size, page_size));
    }
    if is_audio(path) {
        return Some(ffmpeg_picture(path, tier)?.thumbnail(tier, tier));
    }
    if is_video(path) {
        if extension(path).as_deref() == Some("ts") && !is_transport_stream(path) {
            return None;
        }
        let frame = video_frame(path, page_size)?;
        return Some(film_strip(frame));
    }
    let img = if is_ffmpeg_still(path) {
        ffmpeg_picture(path, page_size)?
    } else if is_raw(path) {
        raw_preview(path)?
    } else {
        scaled_jpeg(path, tier).or_else(|| decode_within_limits(path))?
    };
    Some(img.thumbnail(tier, tier))
}

fn too_many_pixels(width: u32, height: u32) -> bool {
    u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
}

/// A picture decoded in full, unless its header claims more than `MAX_IMAGE_PIXELS`.
fn decode_within_limits(path: &Path) -> Option<image::DynamicImage> {
    let open = || image::ImageReader::open(path).ok()?.with_guessed_format().ok();
    let (width, height) = open()?.into_dimensions().ok()?;
    if too_many_pixels(width, height) {
        return None;
    }
    open()?.decode().ok()
}

/// A JPEG decoded at 1/2, 1/4 or 1/8 of its size straight from the DCT coefficients, the way
/// gdk-pixbuf does for Nautilus: a 4K wallpaper costs a fraction of a full decode.
fn scaled_jpeg(path: &Path, tier: u32) -> Option<image::DynamicImage> {
    if !matches!(extension(path).as_deref(), Some("jpg" | "jpeg" | "jpe")) {
        return None;
    }
    let mut decoder = jpeg_decoder::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    decoder.read_info().ok()?;
    // The header says how big the picture is, before anything is allocated for it
    let info = decoder.info()?;
    if too_many_pixels(info.width.into(), info.height.into()) {
        return None;
    }
    let side = tier.min(u16::MAX as u32) as u16;
    decoder.scale(side, side).ok()?;
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => image::RgbImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageRgb8),
        jpeg_decoder::PixelFormat::L8 => image::GrayImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageLuma8),
        _ => None,
    }
}

fn render_svg(path: &Path, px: u32) -> Option<image::DynamicImage> {
    let data = std::fs::read(path).ok()?;
    let mut options = resvg::usvg::Options::default();
    // A picture in a folder must not be able to pull other files into its preview: only what is
    // inside it (`data:` addresses) is drawn, not pictures that `href` names by path
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = resvg::usvg::Tree::from_data(&data, &options).ok()?;
    let size = tree.size();
    let scale = px as f32 / size.width().max(size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new((size.width() * scale).ceil().max(1.0) as u32, (size.height() * scale).ceil().max(1.0) as u32)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let rgba: Vec<u8> = pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    }).collect();
    image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), rgba).map(image::DynamicImage::ImageRgba8)
}

/// A frame from near the start of a video, grabbed with ffmpeg. Skips the first second to avoid
/// black intros, and falls back to the very first frame for clips shorter than that.
fn video_frame(path: &Path, size: u32) -> Option<image::DynamicImage> {
    ["1", "0"].into_iter().find_map(|seek| ffmpeg_frame(path, seek, size))
}

/// One decoded frame at `seek` seconds. Seeks on the input without decoding up to the exact
/// position (the nearest keyframe is plenty for a preview), skips every non-video stream, runs
/// single-threaded because several workers already decode side by side, and writes an
/// uncompressed BMP so no time goes into PNG compression. Pixel aspect ratio is applied, so
/// anamorphic clips keep their shape; rotation metadata is applied by ffmpeg itself.
fn ffmpeg_frame(path: &Path, seek: &str, size: u32) -> Option<image::DynamicImage> {
    use std::process::Command;
    let filter = format!("scale=iw*sar:ih,scale={size}:{size}:force_original_aspect_ratio=decrease,setsar=1");
    let mut command = Command::new("ffmpeg");
    command
        .args(["-nostdin", "-hide_banner", "-v", "quiet", "-threads", "1", "-noaccurate_seek", "-ss", seek, "-i"])
        .arg(path)
        .args(["-an", "-sn", "-dn", "-frames:v", "1", "-vf", &filter, "-f", "image2pipe", "-c:v", "bmp", "-"]);
    image::load_from_memory(&run_with_timeout(command)?).ok()
}

// An audio file's attached cover is read with `ffmpeg_picture`. Most tracks have none, and every
// worker spends its time on the whole folder, so ffmpeg is held to the cheap path: it probes only
// the start of the file (tags sit there), never seeks or decodes audio, maps just the first video
// stream, and scales it down itself so a multi-megapixel cover never travels through the pipe.

/// The first picture stream of a file, scaled to fit `size`: an audio file's attached cover or a
/// still image in a format only ffmpeg reads.
fn ffmpeg_picture(path: &Path, size: u32) -> Option<image::DynamicImage> {
    use std::process::Command;
    let filter = format!("scale={size}:{size}:force_original_aspect_ratio=decrease");
    let mut command = Command::new("ffmpeg");
    command
        .args(["-nostdin", "-hide_banner", "-v", "quiet", "-threads", "1", "-probesize", "1M", "-analyzeduration", "0", "-i"])
        .arg(path)
        .args(["-map", "0:v:0?", "-an", "-sn", "-dn", "-frames:v", "1", "-vf", &filter, "-f", "image2pipe", "-c:v", "bmp", "-"]);
    image::load_from_memory(&run_with_timeout(command)?).ok()
}

/// The largest JPEG inside a RAW file. Every camera maker embeds one (a preview or the full
/// frame), so the file is scanned for JPEG start markers instead of parsing each vendor's container.
fn raw_preview(path: &Path) -> Option<image::DynamicImage> {
    const SOI: [u8; 3] = [0xFF, 0xD8, 0xFF];
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(48 * 1024 * 1024).read_to_end(&mut bytes).ok()?;
    let starts: Vec<usize> = bytes.windows(3).enumerate().filter(|(_, w)| *w == SOI).map(|(i, _)| i).collect();
    // A JPEG runs to the next one's start (or the end), so the largest span is the biggest picture
    let best = starts
        .iter()
        .enumerate()
        .map(|(n, &at)| (at, starts.get(n + 1).copied().unwrap_or(bytes.len())))
        .max_by_key(|(a, b)| b - a)?;
    let jpeg = &bytes[best.0..best.1];
    // The decoder stops at the end-of-image marker, so trailing sensor data does not matter
    image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()
}

/// Stdout of a helper process. A broken or stalled input must not hold a worker forever, so the
/// process is killed after `VIDEO_TIMEOUT`, and so is one that prints more than `MAX_HELPER_OUTPUT`.
fn run_with_timeout(command: std::process::Command) -> Option<Vec<u8>> {
    run_with_limits(command, MAX_HELPER_OUTPUT, VIDEO_TIMEOUT)
}

/// `run_with_timeout` with the limits spelled out: `None` when the process prints more than
/// `max_bytes`, runs longer than `timeout`, or fails to run.
fn run_with_limits(mut command: std::process::Command, max_bytes: u64, timeout: std::time::Duration) -> Option<Vec<u8>> {
    use std::process::Stdio;
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if done_rx.recv_timeout(timeout).is_err() {
            // SAFETY: plain signal to the child this function spawned; a stale pid only fails
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    });
    let mut bytes = Vec::new();
    // One byte more than allowed tells "exactly the limit" from "more"
    let read = child.stdout.take()?.take(max_bytes + 1).read_to_end(&mut bytes);
    let too_much = bytes.len() as u64 > max_bytes;
    if too_much {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = done_tx.send(());
    read.ok()?;
    (!too_much).then_some(bytes)
}

/// Runs work that cannot be interrupted (a render in a library) on a thread of its own, and stops
/// waiting for it after a time. The thread runs on, so only `max_stuck` of those are allowed at
/// once; past that, nothing new is started until some have finished.
struct Guarded {
    stuck: std::sync::atomic::AtomicUsize,
    max_stuck: usize,
}

impl Guarded {
    fn run<T: Send + 'static>(self: &Arc<Self>, timeout: std::time::Duration, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        use std::sync::atomic::AtomicBool;
        if self.stuck.load(Ordering::SeqCst) >= self.max_stuck {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        let (guard, flag) = (self.clone(), abandoned.clone());
        std::thread::Builder::new()
            .name("compression-render".into())
            .spawn(move || {
                let result = work();
                let _ = tx.send(result);
                if flag.load(Ordering::SeqCst) {
                    guard.stuck.fetch_sub(1, Ordering::SeqCst);
                }
            })
            .ok()?;
        match rx.recv_timeout(timeout) {
            Ok(result) => Some(result),
            Err(_) => {
                self.stuck.fetch_add(1, Ordering::SeqCst);
                abandoned.store(true, Ordering::SeqCst);
                None
            }
        }
    }
}

/// Paints sprocket-hole bands over the left and right edges, so a video reads as film.
fn film_strip(frame: image::DynamicImage) -> image::DynamicImage {
    use image::Rgba;
    let mut img = frame.to_rgba8();
    let (w, h) = img.dimensions();
    let band = (w as f32 * 0.09).round().max(6.0) as u32;
    let hole = (band as f32 * 0.5).round().max(3.0) as u32;
    let pitch = hole * 2;
    if w < band * 4 || h < pitch * 2 {
        return image::DynamicImage::ImageRgba8(img);
    }
    for x in (0..band).chain(w - band..w) {
        for y in 0..h {
            img.put_pixel(x, y, Rgba([8, 8, 8, 255]));
        }
    }
    let inset = (band - hole) / 2;
    let mut y = pitch / 2;
    while y + hole <= h {
        for left in [inset, w - band + inset] {
            for dy in 0..hole {
                for dx in 0..hole {
                    img.put_pixel(left + dx, y + dy, Rgba([235, 235, 235, 255]));
                }
            }
        }
        y += pitch;
    }
    image::DynamicImage::ImageRgba8(img)
}

/// First page of a PDF, rendered on the CPU. A page that takes longer than `PDF_TIMEOUT` is given up on.
fn render_pdf(path: &Path, size: u32) -> Option<image::DynamicImage> {
    static RUNAWAY: std::sync::LazyLock<Arc<Guarded>> = std::sync::LazyLock::new(|| Arc::new(Guarded { stuck: Default::default(), max_stuck: MAX_RUNAWAY_PDFS }));
    let path = path.to_path_buf();
    RUNAWAY.run(PDF_TIMEOUT, move || render_pdf_page(&path, size)).flatten()
}

fn render_pdf_page(path: &Path, size: u32) -> Option<image::DynamicImage> {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{PixmapSettings, RenderCache, RenderSettings};

    let pdf = Pdf::new(std::fs::read(path).ok()?).ok()?;
    let page = pdf.pages().first()?;
    let (width, height) = page.render_dimensions();
    let scale = size as f32 / width.max(height).max(1.0);
    let pixmap = hayro::render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE },
    );
    // The page is painted over an opaque white background, so premultiplied equals straight alpha
    let buf = image::RgbaImage::from_raw(pixmap.width() as u32, pixmap.height() as u32, pixmap.data_as_u8_slice().to_vec())?;
    Some(image::DynamicImage::ImageRgba8(buf))
}

/// The cover of an e-book or comic, whichever way its format stores one.
fn book_or_document_cover(path: &Path) -> Option<image::DynamicImage> {
    if is_fb2(path) {
        return fb2_cover(path);
    }
    if is_mobi(path) {
        return mobi_cover(path);
    }
    if is_comic(path) {
        // Comics are routinely misnamed: a .cbr is often a zip and a .cbz sometimes a rar
        return zip_comic_page(path).or_else(|| seven_zip_comic_page(path));
    }
    embedded_preview(path)
}

fn is_page_picture(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !name.contains("__macosx") && [".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp"].iter().any(|e| name.ends_with(e))
}

/// EPUBs and drawing programs carry a ready-made picture of their first page or cover.
fn embedded_preview(path: &Path) -> Option<image::DynamicImage> {
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(path).ok()?)).ok()?;
    let names: Vec<String> = archive.file_names().filter_map(Result::ok).map(|n| n.into_owned()).collect();
    let pick = |wanted: &dyn Fn(&str) -> bool| names.iter().find(|n| wanted(&n.to_ascii_lowercase())).cloned();
    let entry = match extension(path).as_deref() {
        Some("epub") => epub_cover_entry(&mut archive, &names)?,
        Some("kra") => pick(&|n| n == "preview.png" || n == "mergedimage.png")?,
        _ => pick(&|n| n == "thumbnails/thumbnail.png")?,
    };
    read_zip_image(&mut archive, &entry)
}

fn read_zip_image<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>, entry: &str) -> Option<image::DynamicImage> {
    let mut bytes = Vec::new();
    archive.by_name(entry).ok()?.take(32 * 1024 * 1024).read_to_end(&mut bytes).ok()?;
    image::load_from_memory(&bytes).ok()
}

/// Value of `attr="..."` inside one XML tag.
fn xml_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let at = tag.find(&format!("{attr}=\""))? + attr.len() + 2;
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// The cover named by the book's package file, falling back to any picture called "cover".
fn epub_cover_entry<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>, names: &[String]) -> Option<String> {
    let mut read_text = |name: &str| -> Option<String> {
        let mut text = String::new();
        archive.by_name(name).ok()?.take(4 * 1024 * 1024).read_to_string(&mut text).ok()?;
        Some(text)
    };
    let mut by_package = || -> Option<String> {
        let container = read_text("META-INF/container.xml")?;
        let opf_path = xml_attr(&container[container.find("<rootfile")?..], "full-path")?.to_string();
        let opf = read_text(&opf_path)?;
        let base = opf_path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let items: Vec<&str> = opf.split("<item ").skip(1).filter_map(|i| i.split('>').next()).collect();
        let cover_id = opf
            .split("<meta ")
            .skip(1)
            .filter_map(|m| m.split('>').next())
            .find(|m| xml_attr(m, "name") == Some("cover"))
            .and_then(|m| xml_attr(m, "content"));
        let item = items
            .iter()
            .find(|i| xml_attr(i, "properties").is_some_and(|p| p.split_whitespace().any(|p| p == "cover-image")))
            .or_else(|| items.iter().find(|i| cover_id.is_some() && xml_attr(i, "id") == cover_id))?;
        let href = xml_attr(item, "href")?;
        let joined = if base.is_empty() { href.to_string() } else { format!("{base}/{href}") };
        // Package hrefs may be percent-encoded and use ../
        let mut parts: Vec<&str> = Vec::new();
        for part in joined.split('/') {
            match part {
                ".." => {
                    parts.pop();
                }
                "." | "" => {}
                p => parts.push(p),
            }
        }
        let wanted = parts.join("/").replace("%20", " ");
        names.iter().find(|n| **n == wanted).cloned()
    };
    by_package().or_else(|| {
        names
            .iter()
            .find(|n| {
                let n = n.to_ascii_lowercase();
                n.contains("cover") && is_page_picture(&n)
            })
            .cloned()
    })
}

/// First page of a comic stored as a zip.
fn zip_comic_page(path: &Path) -> Option<image::DynamicImage> {
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(path).ok()?)).ok()?;
    let first = archive.file_names().filter_map(Result::ok).filter(|n| is_page_picture(n)).map(|n| n.into_owned()).min_by_key(|n| n.to_ascii_lowercase())?;
    read_zip_image(&mut archive, &first)
}

/// First page of a comic stored as rar or 7z, via the `7z` tool when it is installed.
fn seven_zip_comic_page(path: &Path) -> Option<image::DynamicImage> {
    use std::process::Command;
    if std::fs::metadata(path).ok()?.len() > MAX_DOC_BYTES * 8 {
        return None;
    }
    let tool = ["7z", "7zz", "7za"].into_iter().find(|t| Command::new(t).arg("-h").output().is_ok())?;
    let mut list = Command::new(tool);
    list.args(["l", "-ba", "-slt", "-sccUTF-8", "--"]).arg(path);
    let listing = String::from_utf8(run_with_timeout(list)?).ok()?;
    let first = listing
        .lines()
        .filter_map(|l| l.strip_prefix("Path = "))
        // `@name` would be read as a list of names kept in a file
        .filter(|n| is_page_picture(n) && !n.starts_with('@'))
        .min_by_key(|n| n.to_ascii_lowercase())?
        .to_string();
    image::load_from_memory(&run_with_timeout(seven_zip_extract(tool, path, &first))?).ok()
}

/// `7z e -so -- archive entry`: after `--` a name is a name and never a switch, whatever the
/// archive calls its entry (`-aoa.png`).
fn seven_zip_extract(tool: &str, archive: &Path, entry: &str) -> std::process::Command {
    let mut extract = std::process::Command::new(tool);
    extract.args(["e", "-so", "-y", "--"]).arg(archive).arg(entry);
    extract
}

/// Cover of a FictionBook: a base64 `<binary>` the `<coverpage>` points at. `.fb2.zip` holds the same file zipped.
fn fb2_cover(path: &Path) -> Option<image::DynamicImage> {
    use base64::Engine;
    if std::fs::metadata(path).ok()?.len() > MAX_DOC_BYTES {
        return None;
    }
    let raw = if extension(path).as_deref() == Some("zip") {
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(path).ok()?)).ok()?;
        let name = archive.file_names().filter_map(Result::ok).find(|n| n.to_ascii_lowercase().ends_with(".fb2"))?.into_owned();
        let mut bytes = Vec::new();
        archive.by_name(&name).ok()?.take(MAX_DOC_BYTES).read_to_end(&mut bytes).ok()?;
        bytes
    } else {
        std::fs::read(path).ok()?
    };
    // Only the ASCII markup is searched, so a legacy encoding in the text does not matter
    let text = String::from_utf8_lossy(&raw);
    let cover = text.find("<coverpage").and_then(|at| {
        let page = &text[at..text[at..].find("</coverpage>").map_or(text.len(), |e| at + e)];
        // Matches l:href and xlink:href as well as a bare href
        xml_attr(&page[page.find("<image")?..], "href")
    });
    let id = cover.map(|c| c.trim_start_matches('#'));
    let mut rest = &text[..];
    while let Some(at) = rest.find("<binary") {
        let tag_end = rest[at..].find('>')? + at;
        let body_end = rest[tag_end..].find("</binary>")? + tag_end;
        let tag = &rest[at..tag_end];
        let is_cover = match id {
            Some(id) => xml_attr(tag, "id") == Some(id),
            None => xml_attr(tag, "content-type").is_some_and(|t| t.starts_with("image/")),
        };
        if is_cover {
            let data: String = rest[tag_end + 1..body_end].chars().filter(|c| !c.is_whitespace()).collect();
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
            return image::load_from_memory(&bytes).ok();
        }
        rest = &rest[body_end..];
    }
    None
}

/// Cover of a Kindle/Mobipocket book. The file is a Palm database; an optional EXTH header names the
/// cover as an offset from the first image record.
fn mobi_cover(path: &Path) -> Option<image::DynamicImage> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let u32_at = |b: &[u8], at: usize| b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    let mut head = [0u8; 78];
    file.read_exact(&mut head).ok()?;
    let count = u16::from_be_bytes([head[76], head[77]]) as usize;
    let mut table = vec![0u8; count * 8];
    file.read_exact(&mut table).ok()?;
    let offset = |i: usize| u32_at(&table, i * 8).map(u64::from);
    let len = file.metadata().ok()?.len();
    let record = |file: &mut std::fs::File, i: usize, cap: u64| -> Option<Vec<u8>> {
        let start = offset(i)?;
        let end = if i + 1 < count { offset(i + 1)? } else { len };
        let size = end.checked_sub(start)?.min(cap);
        let mut buf = vec![0u8; size as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut buf).ok()?;
        Some(buf)
    };
    let first = record(&mut file, 0, 1024 * 1024)?;
    if first.get(16..20)? != b"MOBI" {
        return None;
    }
    let header_len = u32_at(&first, 20)? as usize;
    let first_image = u32_at(&first, 108 + 16)? as usize;
    let has_exth = u32_at(&first, 128 + 16)? & 0x40 != 0;
    let mut cover = None;
    if has_exth {
        let exth = 16 + header_len;
        if first.get(exth..exth + 4)? == b"EXTH" {
            let mut at = exth + 12;
            for _ in 0..u32_at(&first, exth + 8)? {
                let (kind, size) = (u32_at(&first, at)?, u32_at(&first, at + 4)? as usize);
                if kind == 201 {
                    cover = u32_at(&first, at + 8);
                }
                if size < 8 {
                    break;
                }
                at += size;
            }
        }
    }
    let index = first_image + cover.unwrap_or(0) as usize;
    if index >= count {
        return None;
    }
    image::load_from_memory(&record(&mut file, index, 16 * 1024 * 1024)?).ok()
}

/// On-disk preview cache under `$XDG_CACHE_HOME/compression/thumbs`. An entry is named by a hash of the
/// file's path, size and modification time, so editing a file makes its old entry unreachable
/// instead of stale. Everything is best effort: any I/O error just means a cache miss.
/// The thumbnail cache shared with Nautilus and other file managers (freedesktop thumbnail
/// specification): `~/.cache/thumbnails/<size>/<md5 of the file's URI>.png`, with the URI and
/// modification time stored in the PNG. Only plain pictures go in, so what other programs find
/// there is the picture itself.
mod shared_cache {
    use std::path::{Path, PathBuf};

    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

    /// What `g_filename_to_uri` leaves unescaped in a path.
    const KEEP: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~').remove(b'!').remove(b'$').remove(b'&').remove(b'\'').remove(b'(').remove(b')').remove(b'*').remove(b'+').remove(b',').remove(b';').remove(b'=').remove(b':').remove(b'@').remove(b'/');

    fn folder(tier: u32) -> Option<&'static str> {
        match tier {
            128 => Some("normal"),
            256 => Some("large"),
            512 => Some("x-large"),
            _ => None,
        }
    }

    fn uri(path: &Path) -> Option<String> {
        use std::os::unix::ffi::OsStrExt;
        let text = std::str::from_utf8(path.as_os_str().as_bytes()).ok()?;
        Some(format!("file://{}", utf8_percent_encode(text, KEEP)))
    }

    fn mtime(path: &Path) -> Option<u64> {
        std::fs::metadata(path).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
    }

    fn entry(uri: &str, tier: u32) -> Option<PathBuf> {
        let name = format!("{:x}.png", md5::compute(uri.as_bytes()));
        Some(dirs::cache_dir()?.join("thumbnails").join(folder(tier)?).join(name))
    }

    pub fn read(path: &Path, tier: u32) -> Option<image::DynamicImage> {
        let uri = uri(path)?;
        let file = entry(&uri, tier)?;
        let bytes = std::fs::read(&file).ok()?;
        let reader = png::Decoder::new(std::io::Cursor::new(&bytes)).read_info().ok()?;
        let text = |key: &str| reader.info().uncompressed_latin1_text.iter().find(|c| c.keyword == key).map(|c| c.text.clone());
        // Made for this very file as it is now, not for an older version or another one with the same name
        if text("Thumb::URI").is_some_and(|u| u != uri) || text("Thumb::MTime")?.trim().parse::<u64>().ok()? != mtime(path)? {
            return None;
        }
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()
    }

    pub fn write(path: &Path, tier: u32, img: &image::DynamicImage) {
        let (Some(uri), Some(mtime)) = (uri(path), mtime(path)) else { return };
        let Some(file) = entry(&uri, tier) else { return };
        let Some(dir) = file.parent() else { return };
        if !dir.exists() {
            use std::os::unix::fs::DirBuilderExt;
            if std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).is_err() {
                return;
            }
        }
        let rgba = img.to_rgba8();
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, rgba.width(), rgba.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let size = std::fs::metadata(path).map_or(0, |m| m.len());
        let ok = encoder.add_text_chunk("Thumb::URI".into(), uri).is_ok()
            && encoder.add_text_chunk("Thumb::MTime".into(), mtime.to_string()).is_ok()
            && encoder.add_text_chunk("Thumb::Size".into(), size.to_string()).is_ok()
            && encoder.write_header().and_then(|mut w| w.write_image_data(rgba.as_raw())).is_ok();
        if !ok {
            return;
        }
        // Written under a temporary name first, so a reader never sees half a file
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        let written = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut f| std::io::Write::write_all(&mut f, &out));
        if written.is_err() || std::fs::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

mod cache {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime};

    /// Bump when the way previews are drawn changes, which orphans every older entry.
    const VERSION: u8 = 3;
    /// The least recently used entries go once the cache passes this size.
    const MAX_BYTES: u64 = 768 * 1024 * 1024;
    /// A file with no preview is tried again after this long, in case ffmpeg was installed meanwhile.
    const MISSING_TTL: Duration = Duration::from_secs(24 * 60 * 60);

    pub struct Key(u64);

    fn root() -> Option<&'static Path> {
        static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
        ROOT.get_or_init(|| dirs::cache_dir().map(|d| d.join("compression").join("thumbs").join(format!("v{VERSION}")))).as_deref()
    }

    pub fn key(path: &Path) -> Option<Key> {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta.modified().ok()?.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        // FNV-1a: unlike `DefaultHasher`, the value is the same in every build and process
        let mut hash = 0xcbf29ce484222325u64;
        let mut feed = |bytes: &[u8]| {
            for b in bytes {
                hash = (hash ^ *b as u64).wrapping_mul(0x100000001b3);
            }
        };
        feed(path.as_os_str().as_bytes());
        feed(&meta.len().to_le_bytes());
        feed(&modified.as_nanos().to_le_bytes());
        Some(Key(hash))
    }

    fn entry(key: &Key, bucket: &str, ext: &str) -> Option<PathBuf> {
        let name = format!("{:016x}", key.0);
        Some(root()?.join(bucket).join(&name[..2]).join(format!("{name}.{ext}")))
    }

    pub fn read(key: &Key, tier: u32) -> Option<image::DynamicImage> {
        let file = entry(key, &tier.to_string(), "png")?;
        let mut bytes = Vec::new();
        std::fs::File::open(&file).ok()?.read_to_end(&mut bytes).ok()?;
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok();
        match img {
            // Marks the entry as recently used for the trimmer
            Some(_) => {
                let _ = std::fs::File::options().write(true).open(&file).and_then(|f| f.set_modified(SystemTime::now()));
            }
            // A truncated write from a crash; let it be rendered again
            None => {
                let _ = std::fs::remove_file(&file);
            }
        }
        img
    }

    pub fn write(key: &Key, tier: u32, img: &image::DynamicImage) {
        let Some(file) = entry(key, &tier.to_string(), "png") else { return };
        let mut png = std::io::Cursor::new(Vec::new());
        let encoder = image::codecs::png::PngEncoder::new_with_quality(&mut png, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::Adaptive);
        if img.write_with_encoder(encoder).is_ok() {
            store(&file, png.get_ref());
        }
    }

    pub fn known_missing(key: &Key) -> bool {
        let Some(marker) = entry(key, "none", "none") else { return false };
        std::fs::metadata(marker).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age < MISSING_TTL))
    }

    pub fn mark_missing(key: &Key) {
        if let Some(marker) = entry(key, "none", "none") {
            store(&marker, &[]);
        }
    }

    /// Written under a temporary name first, so a reader never sees half a file.
    fn store(file: &Path, bytes: &[u8]) {
        let Some(dir) = file.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Deletes the caches of older versions (`v1`, `v2`, ...) next to the current one `current`: their
    /// pictures are never read again. Only real folders named `v` and digits go; the current one, links
    /// and everything else stay.
    fn remove_old_versions(dir: &Path, current: &str) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let numbered = name.strip_prefix('v').is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
            // `DirEntry::file_type` does not follow a link, so a link named like a version is not a folder here
            if numbered && *name != *current && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Deletes the least recently used entries when the cache is over its limit.
    pub fn trim_in_background() {
        std::thread::spawn(|| {
            let Some(root) = root() else { return };
            if let (Some(parent), Some(current)) = (root.parent(), root.file_name()) {
                remove_old_versions(parent, &current.to_string_lossy());
            }
            let mut entries = Vec::new();
            let mut total = 0u64;
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                let Ok(read) = std::fs::read_dir(&dir) else { continue };
                for item in read.flatten() {
                    let Ok(meta) = item.metadata() else { continue };
                    if meta.is_dir() {
                        stack.push(item.path());
                    } else {
                        total += meta.len();
                        entries.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len(), item.path()));
                    }
                }
            }
            if total <= MAX_BYTES {
                return;
            }
            entries.sort();
            // Down to 80% so the next few writes do not trigger another sweep
            for (_, len, path) in entries {
                if total <= MAX_BYTES / 5 * 4 {
                    break;
                }
                if std::fs::remove_file(path).is_ok() {
                    total -= len;
                }
            }
        });
    }
}

