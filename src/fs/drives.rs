//! Removable and extra drives: list them with `lsblk`, mount and eject them with `udisksctl`.
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::places::{Place, PlaceGroup};
use crate::i18n::trf;
use crate::ui::icons;

/// The block device behind a Devices entry of the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    /// `/dev/sdb1`
    pub dev: String,
    pub mounted: bool,
    /// Can be powered off, as USB sticks and cards can.
    pub removable: bool,
    /// Mounted by the system itself (`/boot`, `/home`...), so it is not offered for unmounting.
    pub system: bool,
}

/// Where the desktop and the user put drives. A drive mounted anywhere else was mounted by the system.
const MOUNT_ROOTS: &[&str] = &["/run/media", "/media", "/mnt"];

/// Filesystems that are not something to browse.
const NOT_BROWSABLE: &[&str] = &["swap", "LVM2_member", "crypto_LUKS", "linux_raid_member", "zfs_member", "isw_raid_member", "bcache", "ddf_raid_member"];

/// Partitions that belong to a system's boot rather than hold files, listed only while mounted:
/// EFI system, Microsoft reserved, Windows recovery and BIOS boot (GPT), EFI and hidden recovery (MBR).
const BOOT_PART_TYPES: &[&str] = &[
    "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
    "e3c9e316-0b5c-4db8-817d-f92df00215ae",
    "de94bba4-06d1-4d40-a16a-bfd50179d6ac",
    "21686148-6449-6e6f-744e-656564454649",
    "0xef",
    "0x27",
];

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => s == "1" || s == "true",
        Value::Number(n) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Every folder a device is mounted at: `mountpoints` from newer `lsblk`, else `mountpoint`.
fn mount_points(node: &Value) -> Vec<&str> {
    let mut all: Vec<&str> = node.get("mountpoints").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    if all.is_empty() {
        all.extend(text(node, "mountpoint"));
    }
    all.retain(|m| !m.is_empty() && m.starts_with('/'));
    all
}

fn walk(node: &Value, inherited_removable: bool, all: bool, out: &mut Vec<Place>) {
    let removable = inherited_removable || truthy(&node["rm"]) || truthy(&node["hotplug"]);
    let kind = text(node, "type").unwrap_or_default();
    let fstype = text(node, "fstype");
    let mounts = mount_points(node);
    // A user's mount wins over a system one when the device is mounted at both
    let mount = mounts.iter().find(|m| MOUNT_ROOTS.iter().any(|r| Path::new(m).starts_with(r))).or(mounts.first()).map(PathBuf::from);
    let has_children = node.get("children").and_then(Value::as_array).is_some_and(|c| !c.is_empty());
    let boot_part = text(node, "parttype").is_some_and(|t| BOOT_PART_TYPES.contains(&t.to_ascii_lowercase().as_str()));

    let browsable = fstype.is_some_and(|f| !NOT_BROWSABLE.contains(&f)) && !matches!(kind, "loop" | "rom") && !has_children;
    let user_mount = mount.as_ref().is_some_and(|m| MOUNT_ROOTS.iter().any(|r| m.starts_with(r)));
    // The system's own root is the sidebar's Computer. Without `all`, only what the user plugged in or mounted
    let listed = browsable
        && !mounts.contains(&"/")
        && (mount.is_some() || !boot_part)
        && (all || user_mount || (mount.is_none() && removable));
    if listed && let Some(dev) = text(node, "path") {
        let label = text(node, "label").map(str::to_owned).or_else(|| {
            mount.as_ref().and_then(|m| m.file_name()).map(|n| n.to_string_lossy().into_owned())
        });
        let name = label.unwrap_or_else(|| trf("{} Volume", &[&text(node, "size").unwrap_or("Drive")]));
        let system = mount.is_some() && !user_mount;
        let icon = if removable { icons::USB } else { icons::HARD_DISK };
        let mut place = Place::new(name, mount.clone().unwrap_or_default(), icon, PlaceGroup::Devices);
        place.drive = Some(Drive { dev: dev.to_owned(), mounted: mount.is_some(), removable, system });
        out.push(place);
    }
    for child in node.get("children").and_then(Value::as_array).into_iter().flatten() {
        walk(child, removable, all, out);
    }
}

/// The drives in `lsblk`'s JSON; `all` lists internal disks and partitions too, not just removable ones and user mounts.
pub fn parse_lsblk(json: &str, all: bool) -> Vec<Place> {
    let mut out: Vec<Place> = Vec::new();
    if let Ok(root) = serde_json::from_str::<Value>(json) {
        for node in root["blockdevices"].as_array().into_iter().flatten() {
            walk(node, false, all, &mut out);
        }
    }
    // A device under several holders (RAID, multipath) shows up once per holder
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| p.drive.as_ref().is_none_or(|d| seen.insert(d.dev.clone())));
    // The computer's own disks first, then what is plugged in
    out.sort_by_key(|p| (p.drive.as_ref().is_some_and(|d| d.removable), p.name.to_lowercase()));
    out
}

/// Drives to show, or what sits in the usual mount folders when `lsblk` is missing.
pub fn list(all: bool) -> Vec<Place> {
    // `mountpoints` and `parttype` are missing from old `lsblk`, which then refuses the whole list
    const COLUMNS: [&str; 2] = ["NAME,PATH,LABEL,SIZE,MOUNTPOINT,MOUNTPOINTS,RM,HOTPLUG,TYPE,FSTYPE,PARTTYPE", "NAME,PATH,LABEL,SIZE,MOUNTPOINT,RM,HOTPLUG,TYPE,FSTYPE"];
    for columns in COLUMNS {
        if let Ok(o) = Command::new("lsblk").args(["-J", "-o", columns]).output()
            && o.status.success() {
                return parse_lsblk(&String::from_utf8_lossy(&o.stdout), all);
            }
    }
    from_mount_roots()
}

/// Used and total bytes of the filesystem holding `path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub used: u64,
    pub total: u64,
    /// What an ordinary user can still write, which leaves out the blocks kept for root.
    pub free: u64,
}

impl Space {
    pub fn of(path: &Path) -> Option<Space> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: a valid C string and a statvfs to fill
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let unit = st.f_frsize as u64;
        let total = st.f_blocks as u64 * unit;
        (total > 0).then(|| Space { used: total.saturating_sub(st.f_bfree as u64 * unit), total, free: st.f_bavail as u64 * unit })
    }

    pub fn fraction(&self) -> f32 {
        (self.used as f64 / self.total as f64).clamp(0.0, 1.0) as f32
    }
}

/// Without `lsblk`: whatever sits in the usual mount folders.
fn from_mount_roots() -> Vec<Place> {
    let user = std::env::var("USER").unwrap_or_default();
    let roots = [PathBuf::from("/run/media").join(&user), PathBuf::from("/media").join(&user), PathBuf::from("/media")];
    let mut drives: Vec<Place> = roots
        .iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flat_map(|rd| rd.flatten())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Place::new(name, p, icons::USB, PlaceGroup::Devices)
        })
        .collect();
    drives.sort_by_key(|d| d.name.to_lowercase());
    drives
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program).args(args).output().map_err(|e| trf("{} could not be started: {}", &[&program, &super::describe::io_error(&e)]))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if err.is_empty() { trf("{} failed", &[&program]) } else { err })
    }
}

/// Mount a device; the folder it is mounted at.
pub fn mount(dev: &str) -> Result<PathBuf, String> {
    let said = run("udisksctl", &["mount", "-b", dev])?;
    // "Mounted /dev/sdb1 at /run/media/user/LABEL."
    said.rsplit_once(" at ")
        .map(|(_, p)| PathBuf::from(p.trim().trim_end_matches('.')))
        .ok_or(said)
}

pub fn unmount(dev: &str) -> Result<(), String> {
    unmount_with(dev, &mut run)
}

/// How a program is run: `run`, or what a test puts in its place.
type Runner<'a> = dyn FnMut(&str, &[&str]) -> Result<String, String> + 'a;

fn unmount_with(dev: &str, run: &mut Runner) -> Result<(), String> {
    run("udisksctl", &["unmount", "-b", dev]).map(drop).or_else(|e| run("umount", &[dev]).map(drop).map_err(|_| e))
}

/// The folders the drive that `dev` is part of is mounted on, all its partitions with it: ejecting `dev` powers the
/// whole drive off. `dev` alone when the drive cannot be told (`lsblk` is missing or does not know it).
pub fn mount_points_of_drive(dev: &str) -> Vec<PathBuf> {
    let disk = run("lsblk", &["-no", "pkname", dev]).ok().and_then(|s| s.lines().next().map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("/dev/{s}")));
    let target = disk.as_deref().unwrap_or(dev);
    run("lsblk", &["-J", "-o", "MOUNTPOINTS", target]).map(|text| mount_points_in(&text)).unwrap_or_default()
}

/// Every mount point in `lsblk -J -o MOUNTPOINTS` output, of the devices and all their children.
fn mount_points_in(json: &str) -> Vec<PathBuf> {
    fn walk(node: &serde_json::Value, out: &mut Vec<PathBuf>) {
        if let Some(points) = node.get("mountpoints").and_then(|m| m.as_array()) {
            out.extend(points.iter().filter_map(|p| p.as_str()).filter(|p| !p.is_empty()).map(PathBuf::from));
        }
        for child in node.get("children").and_then(|c| c.as_array()).into_iter().flatten() {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
        for device in value.get("blockdevices").and_then(|d| d.as_array()).into_iter().flatten() {
            walk(device, &mut out);
        }
    }
    out
}

/// Unmounts every file system of the drive `dev` is part of (`dev` first), then powers the drive off so it can be
/// unplugged. When one cannot be unmounted, the drive is not powered off and the error is returned: what was
/// unmounted before stays unmounted, nothing is forced.
pub fn eject(dev: &str) -> Result<(), String> {
    eject_with(dev, &mut run)
}

fn eject_with(dev: &str, run: &mut Runner) -> Result<(), String> {
    let disk = run("lsblk", &["-no", "pkname", dev]).ok().and_then(|s| s.lines().next().map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("/dev/{s}")));
    let disk = disk.as_deref().unwrap_or(dev);
    // When the drive's partitions cannot be listed, only `dev` is known: it alone is unmounted
    let mut devices = run("lsblk", &["-J", "-o", "PATH,MOUNTPOINTS", disk]).map(|text| mounted_devices_in(&text)).unwrap_or_default();
    devices.retain(|d| d != dev);
    devices.insert(0, dev.to_owned());
    for device in &devices {
        unmount_with(device, run).or_else(|e| if is_mounted_with(device, run) { Err(e) } else { Ok(()) })?;
    }
    // Not every device can be powered off (card readers, partitions of internal disks); all of it is unmounted by now
    let _ = run("udisksctl", &["power-off", "-b", disk]);
    Ok(())
}

fn is_mounted_with(dev: &str, run: &mut Runner) -> bool {
    run("lsblk", &["-no", "MOUNTPOINTS", dev]).is_ok_and(|s| !s.trim().is_empty())
}

/// The devices in `lsblk -J -o PATH,MOUNTPOINTS` output that are mounted, of the devices and all their children.
fn mounted_devices_in(json: &str) -> Vec<String> {
    fn walk(node: &serde_json::Value, out: &mut Vec<String>) {
        let mounted = node.get("mountpoints").and_then(|m| m.as_array()).is_some_and(|points| points.iter().any(|p| p.as_str().is_some_and(|p| !p.is_empty())));
        if mounted && let Some(path) = node.get("path").and_then(|p| p.as_str()) {
            out.push(path.to_owned());
        }
        for child in node.get("children").and_then(|c| c.as_array()).into_iter().flatten() {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
        for device in value.get("blockdevices").and_then(|d| d.as_array()).into_iter().flatten() {
            walk(device, &mut out);
        }
    }
    out
}

/// What a failed `udisksctl` call says, in words a person can read. `None` means the person
/// closed the sign-in dialog themselves: that is not an error.
pub fn plain_error(raw: &str) -> Option<String> {
    if raw.contains("NotAuthorizedDismissed") {
        return None;
    }
    // "…: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: the reason" becomes "the reason"
    let said = match raw.split_once("GDBus.Error:") {
        Some((_, rest)) => rest.split_once(": ").map_or(rest, |(_, why)| why),
        None => raw,
    };
    Some(said.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mounted_partition_of_a_drive_is_found_in_the_listing() {
        let json = r#"{"blockdevices": [{"mountpoints": [null], "children": [
            {"mountpoints": ["/run/media/me/PHOTOS"]},
            {"mountpoints": ["/run/media/me/My Data", "/mnt/also"]},
            {"mountpoints": [null]}
        ]}]}"#;
        assert_eq!(mount_points_in(json), vec![PathBuf::from("/run/media/me/PHOTOS"), PathBuf::from("/run/media/me/My Data"), PathBuf::from("/mnt/also")]);
        assert!(mount_points_in("not json").is_empty());
    }

    /// A drive with partitions as `lsblk` lists them, and what `udisksctl` and `umount` do to them, for `eject_with`.
    struct FakeDrive {
        mounted: Vec<&'static str>,
        busy: Vec<&'static str>,
        lsblk_works: bool,
        calls: Vec<String>,
    }

    impl FakeDrive {
        fn new(mounted: &[&'static str]) -> Self {
            FakeDrive { mounted: mounted.to_vec(), busy: Vec::new(), lsblk_works: true, calls: Vec::new() }
        }

        fn run(&mut self, program: &str, args: &[&str]) -> Result<String, String> {
            self.calls.push(format!("{program} {}", args.join(" ")));
            match (program, args) {
                ("lsblk", _) if !self.lsblk_works => Err("lsblk: not found".into()),
                ("lsblk", ["-no", "pkname", _]) => Ok("sdb".into()),
                ("lsblk", ["-J", "-o", "PATH,MOUNTPOINTS", "/dev/sdb"]) => {
                    let child = |p: &str| format!(r#"{{"path": "{p}", "mountpoints": [{}]}}"#, if self.mounted.contains(&p) { format!("\"/run/media/me/{}\"", &p[5..]) } else { "null".into() });
                    let children: Vec<String> = ["/dev/sdb1", "/dev/sdb2", "/dev/sdb3"].iter().map(|p| child(p)).collect();
                    Ok(format!(r#"{{"blockdevices": [{{"path": "/dev/sdb", "mountpoints": [null], "children": [{}]}}]}}"#, children.join(",")))
                }
                ("lsblk", ["-no", "MOUNTPOINTS", dev]) => Ok(if self.mounted.contains(dev) { "/run/media/me/x".into() } else { String::new() }),
                ("udisksctl" | "umount", [.., dev]) if program == "umount" || args[0] == "unmount" => {
                    if self.busy.contains(dev) {
                        return Err(format!("Error unmounting {dev}: target is busy"));
                    }
                    if !self.mounted.contains(dev) {
                        return Err(format!("Error unmounting {dev}: not mounted"));
                    }
                    self.mounted.retain(|m| m != dev);
                    Ok(String::new())
                }
                ("udisksctl", ["power-off", "-b", disk]) => {
                    assert!(self.mounted.is_empty(), "{disk} powered off while {:?} is mounted", self.mounted);
                    Ok(String::new())
                }
                _ => panic!("unexpected {program} {args:?}"),
            }
        }

        fn powered_off(&self) -> bool {
            self.calls.iter().any(|c| c.starts_with("udisksctl power-off"))
        }
    }

    #[test]
    fn ejecting_unmounts_every_partition_of_the_drive_before_powering_it_off() {
        let mut drive = FakeDrive::new(&["/dev/sdb1", "/dev/sdb3"]);
        assert_eq!(eject_with("/dev/sdb3", &mut |p, a| drive.run(p, a)), Ok(()));
        assert!(drive.mounted.is_empty());
        assert!(drive.powered_off());
        // The partition asked for first, then the others
        let unmounts: Vec<&String> = drive.calls.iter().filter(|c| c.starts_with("udisksctl unmount")).collect();
        assert_eq!(unmounts, ["udisksctl unmount -b /dev/sdb3", "udisksctl unmount -b /dev/sdb1"]);
    }

    #[test]
    fn a_partition_that_cannot_be_unmounted_keeps_the_drive_on_and_is_said() {
        let mut drive = FakeDrive::new(&["/dev/sdb1", "/dev/sdb2"]);
        drive.busy.push("/dev/sdb2");
        let result = eject_with("/dev/sdb1", &mut |p, a| drive.run(p, a));
        assert_eq!(result, Err("Error unmounting /dev/sdb2: target is busy".to_owned()));
        assert!(!drive.powered_off());
        // Nothing is forced, and what could be unmounted stays unmounted
        assert!(!drive.calls.iter().any(|c| c.contains(" -f") || c.contains("--force") || c.contains(" -l")), "{:?}", drive.calls);
        assert_eq!(drive.mounted, ["/dev/sdb2"]);
        // The partition asked for itself busy: no other one is touched
        let mut drive = FakeDrive::new(&["/dev/sdb1", "/dev/sdb2"]);
        drive.busy.push("/dev/sdb1");
        assert!(eject_with("/dev/sdb1", &mut |p, a| drive.run(p, a)).is_err());
        assert_eq!(drive.mounted, ["/dev/sdb1", "/dev/sdb2"]);
        assert!(!drive.powered_off());
    }

    #[test]
    fn a_partition_that_was_already_unmounted_or_a_drive_that_cannot_be_listed_still_ejects() {
        // udisks says no, but the partition is not mounted (any more): that is not in the way
        let mut drive = FakeDrive::new(&["/dev/sdb2"]);
        assert_eq!(eject_with("/dev/sdb1", &mut |p, a| drive.run(p, a)), Ok(()));
        assert!(drive.mounted.is_empty() && drive.powered_off());
        // Without lsblk only the partition asked for is known, and the drive is that device
        let mut drive = FakeDrive::new(&["/dev/sdb1"]);
        drive.lsblk_works = false;
        assert_eq!(eject_with("/dev/sdb1", &mut |p, a| drive.run(p, a)), Ok(()));
        assert!(drive.calls.contains(&"udisksctl power-off -b /dev/sdb1".to_owned()), "{:?}", drive.calls);
    }

    #[test]
    fn the_mounted_devices_are_found_in_the_listing() {
        let json = r#"{"blockdevices": [{"path": "/dev/sdb", "mountpoints": [null], "children": [
            {"path": "/dev/sdb1", "mountpoints": ["/run/media/me/PHOTOS"]},
            {"path": "/dev/sdb2", "mountpoints": [null]},
            {"path": "/dev/sdb3", "mountpoints": [""]},
            {"path": "/dev/sdb4", "mountpoints": [null], "children": [{"path": "/dev/mapper/luks-1", "mountpoints": ["/run/media/me/Secret"]}]}
        ]}]}"#;
        assert_eq!(mounted_devices_in(json), ["/dev/sdb1", "/dev/mapper/luks-1"]);
        assert!(mounted_devices_in("not json").is_empty());
    }

    #[test]
    fn a_closed_sign_in_dialog_is_not_an_error_and_other_errors_lose_the_bus_prefix() {
        let dismissed = "Error mounting /dev/sda1: GDBus.Error:org.freedesktop.UDisks2.Error.NotAuthorizedDismissed: The authentication dialog was dismissed";
        assert_eq!(plain_error(dismissed), None);
        let failed = "Error mounting /dev/sdb1 at /run/media/me/data: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: wrong fs type";
        assert_eq!(plain_error(failed).as_deref(), Some("wrong fs type"));
        assert_eq!(plain_error("GDBus.Error:org.freedesktop.UDisks2.Error.Failed").as_deref(), Some("org.freedesktop.UDisks2.Error.Failed"));
        assert_eq!(plain_error("udisksctl: No such file or directory").as_deref(), Some("udisksctl: No such file or directory"));
        assert_eq!(plain_error("  spaced  ").as_deref(), Some("spaced"));
    }

    const SAMPLE: &str = r#"{"blockdevices":[
      {"name":"nvme0n1","path":"/dev/nvme0n1","label":null,"size":"500G","mountpoint":null,"rm":false,"hotplug":false,"type":"disk","fstype":null,"children":[
        {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","label":"boot","size":"1G","mountpoint":"/boot","rm":false,"hotplug":false,"type":"part","fstype":"vfat"},
        {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","label":null,"size":"499G","mountpoint":"/","rm":false,"hotplug":false,"type":"part","fstype":"ext4"}]},
      {"name":"sda","path":"/dev/sda","label":null,"size":"16G","mountpoint":null,"rm":"1","hotplug":"1","type":"disk","fstype":null,"children":[
        {"name":"sda1","path":"/dev/sda1","label":"STICK","size":"8G","mountpoint":"/run/media/me/STICK","rm":"1","hotplug":"1","type":"part","fstype":"vfat"},
        {"name":"sda2","path":"/dev/sda2","label":null,"size":"8G","mountpoint":null,"rm":"1","hotplug":"1","type":"part","fstype":"ext4"}]},
      {"name":"sdb","path":"/dev/sdb","label":null,"size":"1T","mountpoint":null,"rm":false,"hotplug":false,"type":"disk","fstype":null,"children":[
        {"name":"sdb1","path":"/dev/sdb1","label":"data","size":"1T","mountpoint":null,"rm":false,"hotplug":false,"type":"part","fstype":"ext4"}]}]}"#;

    #[test]
    fn lists_every_partition_but_the_root() {
        let drives = parse_lsblk(SAMPLE, true);
        let mut seen: Vec<(&str, bool, bool)> = drives.iter().map(|p| {
            let d = p.drive.as_ref().unwrap();
            (d.dev.as_str(), d.mounted, d.system)
        }).collect();
        seen.sort();
        assert_eq!(seen, vec![
            ("/dev/nvme0n1p1", true, true),
            ("/dev/sda1", true, false),
            ("/dev/sda2", false, false),
            ("/dev/sdb1", false, false),
        ]);
        let stick = drives.iter().find(|p| p.name == "STICK").unwrap();
        assert_eq!(stick.path, PathBuf::from("/run/media/me/STICK"));
        // Internal disks come before the ones plugged in
        assert!(!drives[0].drive.as_ref().unwrap().removable);
    }

    #[test]
    fn lists_a_windows_partition_but_not_its_boot_ones() {
        let json = r#"{"blockdevices":[
          {"name":"nvme1n1","path":"/dev/nvme1n1","label":null,"size":"1T","mountpoint":null,"mountpoints":[null],"rm":false,"hotplug":false,"type":"disk","fstype":null,"parttype":null,"children":[
            {"name":"nvme1n1p1","path":"/dev/nvme1n1p1","label":"SYSTEM","size":"100M","mountpoint":null,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","fstype":"vfat","parttype":"c12a7328-f81f-11d2-ba4b-00a0c93ec93b"},
            {"name":"nvme1n1p2","path":"/dev/nvme1n1p2","label":null,"size":"16M","mountpoint":null,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","fstype":null,"parttype":"e3c9e316-0b5c-4db8-817d-f92df00215ae"},
            {"name":"nvme1n1p3","path":"/dev/nvme1n1p3","label":"Windows","size":"900G","mountpoint":null,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","fstype":"ntfs","parttype":"ebd0a0a2-b9e5-4433-87c0-68b6b72699c7"},
            {"name":"nvme1n1p4","path":"/dev/nvme1n1p4","label":"Recovery","size":"700M","mountpoint":null,"mountpoints":[null],"rm":false,"hotplug":false,"type":"part","fstype":"ntfs","parttype":"de94bba4-06d1-4d40-a16a-bfd50179d6ac"}]},
          {"name":"nvme0n1","path":"/dev/nvme0n1","label":null,"size":"500G","mountpoint":null,"mountpoints":[null],"rm":false,"hotplug":false,"type":"disk","fstype":null,"parttype":null,"children":[
            {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","label":null,"size":"500G","mountpoint":"/home","mountpoints":["/home","/nix","/"],"rm":false,"hotplug":false,"type":"part","fstype":"btrfs","parttype":"0fc63daf-8483-4772-8e79-3d69d8477de4"}]}]}"#;
        let drives = parse_lsblk(json, true);
        assert!(parse_lsblk(json, false).is_empty());
        let names: Vec<&str> = drives.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Windows"]);
        let windows = drives[0].drive.as_ref().unwrap();
        assert!(!windows.mounted && !windows.removable);
        assert_eq!(drives[0].icon, icons::HARD_DISK);
    }

    #[test]
    fn without_all_lists_user_mounts_and_unmounted_removables_only() {
        let mut seen: Vec<String> = parse_lsblk(SAMPLE, false).iter().map(|p| p.drive.as_ref().unwrap().dev.clone()).collect();
        seen.sort();
        assert_eq!(seen, vec!["/dev/sda1", "/dev/sda2"]);
    }

    #[test]
    fn space_of_the_root() {
        let space = Space::of(Path::new("/")).unwrap();
        assert!(space.total > 0 && space.used <= space.total && space.free <= space.total);
    }

    #[test]
    fn a_program_that_cannot_be_started_is_told_in_words_and_with_its_name() {
        let err = run("/nonexistent/program-for-test", &[]).unwrap_err();
        assert_eq!(err, "/nonexistent/program-for-test could not be started: Not found");
        assert!(!err.contains("os error"));
        // One that runs and fails without a word says so in ours
        assert_eq!(run("false", &[]).unwrap_err(), "false failed");
    }
}
