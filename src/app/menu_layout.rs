//! The person's own order of the context menus of a file, of a folder and of the empty space (Settings → Context menu).
//! Every entry has an id that stays the same in every language; a layout lists those ids in groups and submenus, some of
//! them hidden. The menu is still made as always (what applies to the item, what is greyed out); the layout only says where
//! each of its entries goes. What the layout does not know (a plugin switched on later, a new action) keeps its place in a
//! group of its own, so nothing goes missing without the person having hidden it.
use super::*;
use crate::settings::{MenuLayout, MenuLeaf, MenuNode};

/// Which of the menus that can be arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuKind {
    File,
    Folder,
    Background,
}

impl MenuKind {
    pub(crate) const ALL: [MenuKind; 3] = [MenuKind::File, MenuKind::Folder, MenuKind::Background];

    pub(crate) fn label(self) -> &'static str {
        match self {
            MenuKind::File => tr("Files"),
            MenuKind::Folder => tr("Folders"),
            MenuKind::Background => tr("Empty space"),
        }
    }
}

/// The id of a menu entry, from what it does; `None` for a submenu (its entries have ids) and for what cannot be arranged.
pub(super) fn entry_id(action: &MenuAction) -> Option<String> {
    let id = match action {
        MenuAction::Open(_) => "open",
        MenuAction::OpenWith(_) => "open_with",
        MenuAction::OpenPath(_) => "show_in_folder",
        MenuAction::Bookmark(_) | MenuAction::Unbookmark(_) => "bookmark",
        MenuAction::Pin(_) | MenuAction::Unpin(_) => "pin",
        MenuAction::UseAsIcon(_) => "use_as_icon",
        MenuAction::SetIcon(_) => "icon_image",
        MenuAction::SetGlyph(_) => "icon_glyph",
        MenuAction::Copy(_) => "copy",
        MenuAction::Cut(_) => "cut",
        MenuAction::Rename(_) | MenuAction::RenameMany(_) => "rename",
        MenuAction::Trash(_) => "trash",
        MenuAction::DeleteForever(_) => "delete",
        MenuAction::CopyTo(_) => "copy_to",
        MenuAction::MoveTo(_) => "move_to",
        MenuAction::Duplicate(_) => "duplicate",
        MenuAction::MakeLink(_) => "make_link",
        MenuAction::Compress(_) => "compress",
        MenuAction::ExtractHere(_) => "extract_here",
        MenuAction::ExtractTo(_) => "extract_to",
        MenuAction::OpenInNewTab(_) => "new_tab",
        MenuAction::OpenInNewWindow(_) => "new_window",
        MenuAction::OpenTerminal(_) => "terminal",
        MenuAction::OpenAsAdmin(_) => "admin",
        MenuAction::CalcSize(_) => "calc_size",
        MenuAction::CopyPath(_) => "copy_path",
        MenuAction::Properties(_) => "properties",
        MenuAction::NewFile => "new_file",
        MenuAction::NewFolder => "new_folder",
        MenuAction::Undo => "undo",
        MenuAction::Redo => "redo",
        MenuAction::Paste => "paste",
        MenuAction::SelectAll => "select_all",
        MenuAction::InvertSelection => "invert_selection",
        MenuAction::SelectPattern => "select_pattern",
        MenuAction::Refresh => "refresh",
        MenuAction::Plugin(plugin, id, ..) => return Some(plugin_entry_id(plugin, id)),
        _ => return None,
    };
    Some(id.to_owned())
}

/// The id of a plugin's entry: `plugin:<plugin>:<entry>`. Entries that stand for a family (one per tag) share one id
/// ending in `*`, as the plugin's catalog lists them.
pub(crate) fn plugin_entry_id(plugin: &str, id: &str) -> String {
    match id.split_once(':') {
        Some((family, _)) if plugin == "tags" && family == "toggle" => format!("plugin:{plugin}:toggle:*"),
        _ => format!("plugin:{plugin}:{id}"),
    }
}

/// The icon and name of a built-in entry, as its settings show it; `None` for an id that is not built in.
pub(crate) fn builtin_entry(id: &str) -> Option<(&'static str, &'static str)> {
    Some(match id {
        "open" => (icons::OPEN, tr("Open")),
        "open_with" => (icons::APPS, tr("Open with…")),
        "show_in_folder" => (icons::FOLDER, tr("Show in folder")),
        "bookmark" => (icons::HEART_OUTLINE, tr("Add to bookmarks")),
        "pin" => (icons::PIN, tr("Pin to sidebar")),
        "use_as_icon" => (icons::IMAGE, tr("Use as folder icon")),
        "icon_image" => (icons::IMAGE, tr("Image…")),
        "icon_glyph" => (icons::PALETTE, tr("Glyph…")),
        "copy" => (icons::COPY, tr("Copy")),
        "cut" => (icons::CUT, tr("Cut")),
        "rename" => (icons::RENAME, tr("Rename")),
        "trash" => (icons::TRASH, tr("Move to trash")),
        "delete" => (icons::DELETE_FOREVER, tr("Delete permanently")),
        "copy_to" => (icons::FOLDER_COPY, tr("Copy to…")),
        "move_to" => (icons::FOLDER_MOVE, tr("Move to…")),
        "duplicate" => (icons::DUPLICATE, tr("Duplicate")),
        "make_link" => (icons::LINK, tr("Create link")),
        "compress" => (icons::COMPRESS, tr("Compress…")),
        "extract_here" => (icons::EXTRACT, tr("Extract here")),
        "extract_to" => (icons::EXTRACT, tr("Extract to…")),
        "new_tab" => (icons::NEW_TAB, tr("New tab")),
        "new_window" => (icons::NEW_WINDOW, tr("New window")),
        "terminal" => (icons::TERMINAL, tr("Terminal")),
        "admin" => (icons::SHIELD, tr("Open as administrator")),
        "calc_size" => (icons::STORAGE, tr("Calculate size")),
        "copy_path" => (icons::COPY, tr("Copy path")),
        "properties" => (icons::INFO, tr("Properties")),
        "new_file" => (icons::FILE_PLUS, tr("New file")),
        "new_folder" => (icons::FOLDER_PLUS, tr("New folder")),
        "undo" => (icons::UNDO, tr("Undo")),
        "redo" => (icons::REDO, tr("Redo")),
        "paste" => (icons::PASTE, tr("Paste")),
        "select_all" => (icons::SELECT_ALL, tr("Select all")),
        "invert_selection" => (icons::SELECT_INVERSE, tr("Invert selection")),
        "select_pattern" => (icons::SELECT_PATTERN, tr("Select matching…")),
        "refresh" => (icons::REFRESH, tr("Refresh")),
        _ => return None,
    })
}

/// The name an entry has when it stands alone although it was made for a submenu ("New tab" under "Open in"): a submenu
/// left with one entry shows that entry, under a name that says all of it.
/// Built-in entries have a name of their own for that; the entries of the built-in "More actions" and "Select" say all
/// there is already. Any other (of a plugin's submenu, of one the person named) takes its submenu's name in front:
/// "Git: Stage", "Mine: Copy".
fn full_label(id: &str, label: &'static str, submenu: Option<&str>) -> &'static str {
    match id {
        "new_tab" => tr("Open in new tab"),
        "new_window" => tr("Open in new window"),
        "terminal" => tr("Open in terminal"),
        "icon_image" => tr("Custom icon: image…"),
        "icon_glyph" => tr("Custom icon: glyph…"),
        _ => match submenu {
            Some(name) if name != tr("More actions") && name != tr("Select") && !name.is_empty() => crate::plugins::intern(&trf("{}: {}", &[&name, &label])),
            _ => label,
        },
    }
}

/// The name of a submenu: the person's own, or that of a built-in or plugin submenu in the window's language.
pub(crate) fn submenu_label(name: &str, plugin_groups: &[(String, &'static str)]) -> &'static str {
    match name {
        "builtin:open_in" => tr("Open in"),
        "builtin:more" => tr("More actions"),
        "builtin:select" => tr("Select"),
        "builtin:custom_icon" => tr("Custom icon"),
        _ => match name.strip_prefix("plugin-group:").and_then(|g| plugin_groups.iter().find(|(id, _)| id == g)) {
            Some((_, label)) => label,
            None => crate::plugins::intern(name.strip_prefix("plugin-group:").unwrap_or(name)),
        },
    }
}

fn entry(id: &str) -> MenuNode {
    MenuNode::Entry { id: id.to_owned(), hidden: false }
}

fn submenu(name: &str, ids: &[&str]) -> MenuNode {
    MenuNode::Submenu { name: name.to_owned(), hidden: false, entries: ids.iter().map(|id| MenuLeaf { id: (*id).to_owned(), hidden: false }).collect() }
}

/// The plugins' entries as layout nodes: entries of one group in a submenu named after the group, the others alone.
fn plugin_nodes(catalog: &[(&'static str, crate::plugins::MenuItem)]) -> Vec<MenuNode> {
    let mut nodes: Vec<MenuNode> = Vec::new();
    for (plugin, item) in catalog {
        let id = plugin_entry_id(plugin, &item.id);
        match item.group {
            Some((group, _)) => {
                let name = format!("plugin-group:{group}");
                match nodes.iter_mut().find(|n| matches!(n, MenuNode::Submenu { name: n, .. } if *n == name)) {
                    Some(MenuNode::Submenu { entries, .. }) => entries.push(MenuLeaf { id, hidden: false }),
                    _ => nodes.push(MenuNode::Submenu { name, hidden: false, entries: vec![MenuLeaf { id, hidden: false }] }),
                }
            }
            None => nodes.push(MenuNode::Entry { id, hidden: false }),
        }
    }
    nodes
}

/// The built-in arrangement of a menu, with what the enabled plugins add (`catalog`): where a new layout starts from.
pub(crate) fn default_layout(kind: MenuKind, catalog: &[(&'static str, crate::plugins::MenuItem)]) -> MenuLayout {
    let manage = |extract: bool| {
        let mut more = vec!["duplicate", "make_link", "compress"];
        if extract {
            more.extend(["extract_here", "extract_to"]);
        }
        vec![entry("rename"), entry("trash"), entry("delete"), entry("copy_to"), entry("move_to"), submenu("builtin:more", &more)]
    };
    let plugins = plugin_nodes(catalog);
    let groups = match kind {
        MenuKind::File => vec![
            vec![entry("open"), entry("open_with"), entry("show_in_folder")],
            vec![entry("bookmark"), entry("pin"), entry("use_as_icon")],
            vec![entry("copy"), entry("cut")],
            manage(true),
            plugins,
            vec![entry("copy_path"), entry("properties")],
        ],
        MenuKind::Folder => vec![
            vec![entry("open"), submenu("builtin:open_in", &["new_tab", "new_window", "terminal", "admin"]), entry("show_in_folder")],
            vec![entry("bookmark"), entry("pin"), submenu("builtin:custom_icon", &["icon_image", "icon_glyph"])],
            vec![entry("copy"), entry("cut")],
            manage(false),
            plugins,
            vec![entry("calc_size"), entry("copy_path"), entry("properties")],
        ],
        MenuKind::Background => vec![
            vec![entry("new_file"), entry("new_folder")],
            plugins,
            vec![entry("undo"), entry("redo")],
            vec![entry("paste"), submenu("builtin:select", &["select_all", "invert_selection", "select_pattern"])],
            vec![entry("bookmark"), entry("pin"), submenu("builtin:custom_icon", &["icon_image", "icon_glyph"]), entry("terminal"), entry("admin")],
            vec![entry("refresh")],
            vec![entry("copy_path"), entry("properties")],
        ],
    };
    MenuLayout { groups: groups.into_iter().filter(|g| !g.is_empty()).collect() }
}

/// An entry of the menu as it was made, with its id and where it was: the index of its top-level node, so what the
/// layout does not know can stay together as it came.
struct Made {
    id: Option<String>,
    entry: MenuEntry,
    top: usize,
}

/// Every id a layout names, hidden ones too.
fn named(layout: &MenuLayout) -> Vec<&str> {
    let mut out = Vec::new();
    for node in layout.groups.iter().flatten() {
        match node {
            MenuNode::Entry { id, .. } => out.push(id.as_str()),
            MenuNode::Submenu { entries, .. } => out.extend(entries.iter().map(|l| l.id.as_str())),
        }
    }
    out
}

/// The id of a made entry. A greyed-out entry ("Paste" with nothing to paste) has no action to tell it by: its name does.
fn made_id(entry: &MenuEntry) -> Option<String> {
    match &entry.2 {
        MenuAction::Unavailable(_) => ["paste"].into_iter().find(|id| builtin_entry(id).is_some_and(|(_, label)| label == entry.1)).map(str::to_owned),
        action => entry_id(action),
    }
}

/// Puts the entries of `groups` (the menu as it was made) where `layout` says. `plugin_groups` names the plugins' submenus.
pub(super) fn apply(groups: Vec<Vec<MenuEntry>>, layout: &MenuLayout, plugin_groups: &[(String, &'static str)]) -> Vec<Vec<MenuEntry>> {
    // Flat, submenus opened up, each entry knowing the top-level node it came from
    let mut made: Vec<Made> = Vec::new();
    let mut tops: Vec<MenuEntry> = Vec::new();
    let mut top_group: Vec<usize> = Vec::new();
    for (g, group) in groups.into_iter().enumerate() {
        for node in group {
            let top = tops.len();
            if let MenuAction::Submenu(children) = &node.2 {
                for child in children {
                    made.push(Made { id: made_id(child), entry: child.clone(), top });
                }
            } else {
                made.push(Made { id: made_id(&node), entry: node.clone(), top });
            }
            tops.push(node);
            top_group.push(g);
        }
    }
    let mut taken = vec![false; made.len()];
    let take = |id: &str, made: &[Made], taken: &mut Vec<bool>| -> Vec<(String, MenuEntry)> {
        let family = id.strip_suffix('*');
        let mut out = Vec::new();
        for (i, m) in made.iter().enumerate() {
            let fits = m.id.as_deref().is_some_and(|mid| mid == id || family.is_some_and(|f| mid.starts_with(f)));
            if fits && !taken[i] {
                taken[i] = true;
                out.push((m.id.clone().unwrap_or_default(), m.entry.clone()));
            }
        }
        out
    };
    let mut out: Vec<Vec<MenuEntry>> = Vec::new();
    for group in &layout.groups {
        let mut placed = Vec::new();
        for node in group {
            match node {
                MenuNode::Entry { id, hidden } => {
                    let found = take(id, &made, &mut taken);
                    if !hidden {
                        placed.extend(found.into_iter().map(|(_, e)| e));
                    }
                }
                MenuNode::Submenu { name, hidden, entries } => {
                    let mut children: Vec<(String, MenuEntry)> = Vec::new();
                    for leaf in entries {
                        let found = take(&leaf.id, &made, &mut taken);
                        if !leaf.hidden {
                            children.extend(found);
                        }
                    }
                    if *hidden {
                        continue;
                    }
                    match children.len() {
                        0 => {}
                        // One entry left: it stands for itself, under its whole name
                        1 => {
                            let (id, (icon, label, action)) = children.remove(0);
                            placed.push((icon, full_label(&id, label, Some(submenu_label(name, plugin_groups))), action));
                        }
                        _ => {
                            let icon = children[0].1.0;
                            placed.push((icon, submenu_label(name, plugin_groups), MenuAction::Submenu(children.into_iter().map(|(_, e)| e).collect())));
                        }
                    }
                }
            }
        }
        out.push(placed);
    }
    // What the layout does not name keeps its place as it came, in groups before the last: a whole node where none of
    // it is named, else what is left of it, each entry under its whole name
    let known = named(layout);
    let unknown = |i: usize| !taken[i] && made[i].id.as_deref().is_none_or(|id| !known.iter().any(|k| *k == id || k.strip_suffix('*').is_some_and(|f| id.starts_with(f))));
    let mut rest: Vec<Vec<MenuEntry>> = Vec::new();
    let mut last_group = usize::MAX;
    for (top, node) in tops.into_iter().enumerate() {
        let ours: Vec<usize> = (0..made.len()).filter(|&i| made[i].top == top).collect();
        let piece: Vec<MenuEntry> = if !ours.is_empty() && ours.iter().all(|&i| unknown(i)) {
            vec![node]
        } else {
            // The submenu it came out of is named in front of it (see `full_label`)
            let from = matches!(node.2, MenuAction::Submenu(_)).then_some(node.1);
            ours.iter().filter(|&&i| unknown(i)).map(|&i| {
                let (icon, label, action) = made[i].entry.clone();
                (icon, full_label(made[i].id.as_deref().unwrap_or_default(), label, from), action)
            }).collect()
        };
        if piece.is_empty() {
            continue;
        }
        if top_group[top] != last_group {
            rest.push(Vec::new());
            last_group = top_group[top];
        }
        if let Some(group) = rest.last_mut() {
            group.extend(piece);
        }
    }
    let at = out.len().saturating_sub(1);
    for (n, group) in rest.into_iter().filter(|g| !g.is_empty()).enumerate() {
        out.insert(at + n, group);
    }
    out.retain(|g| !g.is_empty());
    out
}

/// Where an entry or submenu of a layout is: its group, its place there, and its place inside a submenu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spot {
    pub group: usize,
    pub index: usize,
    pub leaf: Option<usize>,
}

/// Where a dragged entry is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drop {
    /// In front of the node at `index` of `group` (or of the entry `leaf` of the submenu there); an index past the end
    /// puts it last.
    Before(Spot),
    /// At the end of the submenu at `index` of `group`.
    Into { group: usize, index: usize },
}

/// Moves the node at `from` to `to`. An entry goes into and out of submenus as it is dropped; a submenu goes only between
/// groups (a submenu in a submenu is not offered by the menus). Nothing happens for a drop that makes no sense.
pub(crate) fn move_node(layout: &mut MenuLayout, from: Spot, to: Drop) {
    let groups = &mut layout.groups;
    let Some(node) = groups.get(from.group).and_then(|g| g.get(from.index)) else { return };
    let moving_submenu = matches!(node, MenuNode::Submenu { .. }) && from.leaf.is_none();
    let into_submenu = matches!(to, Drop::Into { .. }) || matches!(to, Drop::Before(Spot { leaf: Some(_), .. }));
    if moving_submenu && into_submenu {
        return;
    }
    if let Drop::Into { group, index } | Drop::Before(Spot { group, index, leaf: Some(_) }) = to
        && !matches!(groups.get(group).and_then(|g| g.get(index)), Some(MenuNode::Submenu { .. }))
    {
        return;
    }
    // Out of where it is
    let (id, hidden, whole) = match from.leaf {
        Some(leaf) => {
            let Some(MenuNode::Submenu { entries, .. }) = groups[from.group].get_mut(from.index) else { return };
            if leaf >= entries.len() {
                return;
            }
            let MenuLeaf { id, hidden } = entries.remove(leaf);
            (id, hidden, None)
        }
        None => match groups[from.group].remove(from.index) {
            MenuNode::Entry { id, hidden } => (id, hidden, None),
            submenu => (String::new(), false, Some(submenu)),
        },
    };
    // Where it goes; places after the one taken out moved up by one
    let shift = |group: usize, index: usize, leaf: Option<usize>| -> (usize, Option<usize>) {
        match (from.leaf, leaf) {
            (None, _) if group == from.group && index > from.index => (index - 1, leaf),
            (Some(fl), Some(l)) if group == from.group && index == from.index && l > fl => (index, Some(l - 1)),
            _ => (index, leaf),
        }
    };
    match to {
        Drop::Into { group, index } => {
            let (index, _) = shift(group, index, None);
            if let Some(MenuNode::Submenu { entries, .. }) = groups.get_mut(group).and_then(|g| g.get_mut(index)) {
                entries.push(MenuLeaf { id, hidden });
            }
        }
        Drop::Before(Spot { group, index, leaf }) => {
            let (index, leaf) = shift(group, index, leaf);
            let Some(target) = groups.get_mut(group) else { return };
            match leaf {
                Some(leaf) => {
                    if let Some(MenuNode::Submenu { entries, .. }) = target.get_mut(index) {
                        entries.insert(leaf.min(entries.len()), MenuLeaf { id, hidden });
                    }
                }
                None => target.insert(index.min(target.len()), whole.unwrap_or(MenuNode::Entry { id, hidden })),
            }
        }
    }
}

/// What is being dragged on the settings page, and from where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dragged(Spot);

const ROW_H: f32 = 34.0;
const EYE_W: f32 = 40.0;

/// The layout `layout` with what the enabled plugins offer (`catalog`) and it does not name yet: in a group before the
/// last, as the menu shows them.
pub(crate) fn with_new_plugin_entries(mut layout: MenuLayout, catalog: &[(&'static str, crate::plugins::MenuItem)]) -> MenuLayout {
    let known: Vec<String> = named(&layout).into_iter().map(str::to_owned).collect();
    let fresh: Vec<MenuNode> = plugin_nodes(catalog)
        .into_iter()
        .filter_map(|node| match node {
            MenuNode::Entry { ref id, .. } => (!known.contains(id)).then_some(node),
            MenuNode::Submenu { name, hidden, entries } => {
                let entries: Vec<MenuLeaf> = entries.into_iter().filter(|l| !known.contains(&l.id)).collect();
                (!entries.is_empty()).then_some(MenuNode::Submenu { name, hidden, entries })
            }
        })
        .collect();
    if !fresh.is_empty() {
        let at = layout.groups.len().saturating_sub(1);
        layout.groups.insert(at, fresh);
    }
    layout
}

impl CompressionApp {
    /// Settings → Context menu: the menus of a file, of a folder and of the empty space, arranged by dragging entries
    /// (between groups, into and out of submenus), hidden with the eye, grouped into submenus with names of their own.
    pub(crate) fn context_menu_tab(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme;
        let tab_id = egui::Id::unique("context_menu_tab");
        let mut tab = ui.ctx().data(|d| d.get_temp::<usize>(tab_id)).unwrap_or(0).min(MenuKind::ALL.len() - 1);
        let names: Vec<&str> = MenuKind::ALL.iter().map(|k| k.label()).collect();
        if let Some(picked) = material::segmented(ui, "context_menu_kind", &names, tab, &theme) {
            tab = picked;
            ui.ctx().data_mut(|d| d.insert_temp(tab_id, tab));
        }
        let kind = MenuKind::ALL[tab];
        let catalog = self.plugins.menu_catalog(kind == MenuKind::Background);
        let plugin_groups = self.plugin_group_names();
        let custom = self.menu_layout(kind).is_some();
        let mut layout = with_new_plugin_entries(self.menu_layout(kind).cloned().unwrap_or_else(|| default_layout(kind, &catalog)), &catalog);
        ui.label(egui::RichText::new(tr("Drag entries to change their order, between groups and into or out of submenus. The eye hides an entry.")).size(material::FONT_CAPTION).color(theme.subtext0));

        let describe = |id: &str| -> Option<(&'static str, &'static str)> {
            builtin_entry(id).or_else(|| catalog.iter().find(|(plugin, item)| plugin_entry_id(plugin, &item.id) == id).map(|(_, item)| (item.icon, item.label)))
        };
        let mut changed = false;
        let mut drops: Vec<(egui::Rect, Drop)> = Vec::new();
        let dragged = egui::DragAndDrop::payload::<Dragged>(ui.ctx()).map(|d| d.0);
        let mut remove_group = None;
        for g in 0..layout.groups.len() {
            material::group_card(ui, &theme, |ui| {
                if layout.groups[g].is_empty() {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::hover());
                    ui.painter().text(rect.left_center() + egui::vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, tr("Drop entries here"), egui::FontId::proportional(material::FONT_BODY), theme.subtext0);
                    drops.push((rect, Drop::Before(Spot { group: g, index: 0, leaf: None })));
                    if material::button(ui, ("cm_remove_group", g), Some(icons::CLOSE), tr("Remove group"), &theme, material::ButtonKind::Text, true).clicked() {
                        remove_group = Some(g);
                    }
                    return;
                }
                for i in 0..layout.groups[g].len() {
                    let spot = Spot { group: g, index: i, leaf: None };
                    // An entry of a plugin that is off is kept, and not shown
                    if let MenuNode::Entry { id, .. } = &layout.groups[g][i]
                        && id.starts_with("plugin:")
                        && describe(id).is_none()
                    {
                        continue;
                    }
                    let (rect, eye) = match &layout.groups[g][i] {
                        MenuNode::Entry { id, hidden } => {
                            let (icon, label) = describe(id).unwrap_or((icons::MORE, crate::plugins::intern(id)));
                            entry_row(ui, &theme, (icon, label), *hidden, spot, dragged == Some(spot), 0.0)
                        }
                        MenuNode::Submenu { name, hidden, .. } => submenu_row(ui, &theme, submenu_label(name, &plugin_groups), *hidden, spot, dragged == Some(spot)),
                    };
                    if eye {
                        toggle_hidden(&mut layout, spot);
                        changed = true;
                    }
                    let is_submenu = matches!(layout.groups[g][i], MenuNode::Submenu { .. });
                    if is_submenu {
                        // The upper part puts it in front; the rest drops into the submenu
                        let (upper, lower) = rect.split_top_bottom_at_y(rect.top() + rect.height() * 0.35);
                        drops.push((upper, Drop::Before(spot)));
                        drops.push((lower, Drop::Into { group: g, index: i }));
                        let leaves = match &layout.groups[g][i] {
                            MenuNode::Submenu { entries, .. } => entries.clone(),
                            MenuNode::Entry { .. } => Vec::new(),
                        };
                        for (j, leaf) in leaves.iter().enumerate() {
                            let leaf_spot = Spot { group: g, index: i, leaf: Some(j) };
                            if leaf.id.starts_with("plugin:") && describe(&leaf.id).is_none() {
                                continue;
                            }
                            let (icon, label) = describe(&leaf.id).unwrap_or((icons::MORE, "?"));
                            let (leaf_rect, leaf_eye) = entry_row(ui, &theme, (icon, label), leaf.hidden, leaf_spot, dragged == Some(leaf_spot), 28.0);
                            if leaf_eye {
                                toggle_hidden(&mut layout, leaf_spot);
                                changed = true;
                            }
                            drops.push((leaf_rect, Drop::Before(leaf_spot)));
                        }
                    } else {
                        drops.push((rect, Drop::Before(spot)));
                    }
                }
                // Below the last row: the end of the group
                let (rest, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::hover());
                drops.push((rest, Drop::Before(Spot { group: g, index: layout.groups[g].len(), leaf: None })));
            });
        }
        if let Some(g) = remove_group {
            layout.groups.remove(g);
            changed = true;
        }
        // The drop: where the pointer is let go
        if let Some(from) = dragged {
            let pointer = ui.ctx().pointer_latest_pos();
            let target = pointer.and_then(|p| drops.iter().find(|(r, _)| r.contains(p))).map(|(r, d)| (*r, *d));
            if let Some((rect, drop)) = target {
                let y = match drop {
                    Drop::Into { .. } => rect.center().y,
                    Drop::Before(_) => rect.top(),
                };
                ui.painter().hline(rect.x_range(), y, egui::Stroke::new(2.0, theme.mauve));
            }
            if ui.input(|i| i.pointer.any_released()) {
                egui::DragAndDrop::clear_payload(ui.ctx());
                if let Some((_, drop)) = target {
                    move_node(&mut layout, from, drop);
                    changed = true;
                }
            }
        }
        ui.horizontal(|ui| {
            if material::button(ui, "cm_new_group", Some(icons::ADD), tr("New group"), &theme, material::ButtonKind::Tonal, true).clicked() {
                layout.groups.push(Vec::new());
                changed = true;
            }
            // Next to it, so the way back is where the changes are made
            if material::button(ui, "cm_reset", Some(icons::RESTORE), tr("Reset"), &theme, material::ButtonKind::Text, custom).clicked() {
                self.set_menu_layout(kind, None);
                return;
            }
            if material::button(ui, "cm_new_submenu", Some(icons::FOLDER_PLUS), tr("New submenu"), &theme, material::ButtonKind::Text, true).clicked() {
                let last = layout.groups.len().saturating_sub(1);
                if layout.groups.is_empty() {
                    layout.groups.push(Vec::new());
                }
                layout.groups[last].push(MenuNode::Submenu { name: tr("New submenu").to_owned(), hidden: false, entries: Vec::new() });
                changed = true;
            }
        });
        let renamed = take_renames(ui.ctx(), &mut layout);
        if changed || renamed {
            self.set_menu_layout(kind, Some(layout));
        }
    }

    /// Keeps the person's layout of a menu (`None`: the built-in one again) and writes the settings.
    pub(crate) fn set_menu_layout(&mut self, kind: MenuKind, layout: Option<MenuLayout>) {
        let layouts = &mut self.settings.menu_layouts;
        match kind {
            MenuKind::File => layouts.file = layout,
            MenuKind::Folder => layouts.folder = layout,
            MenuKind::Background => layouts.background = layout,
        }
        self.persist_settings();
    }
}

/// Flips the eye of the node at `spot`.
fn toggle_hidden(layout: &mut MenuLayout, spot: Spot) {
    let Some(node) = layout.groups.get_mut(spot.group).and_then(|g| g.get_mut(spot.index)) else { return };
    match (node, spot.leaf) {
        (MenuNode::Entry { hidden, .. }, None) | (MenuNode::Submenu { hidden, .. }, None) => *hidden = !*hidden,
        (MenuNode::Submenu { entries, .. }, Some(leaf)) => {
            if let Some(l) = entries.get_mut(leaf) {
                l.hidden = !l.hidden;
            }
        }
        _ => {}
    }
}

/// A row of an entry: the part with the handle and the name can be dragged, the eye only clicked (a press on the eye never
/// starts a drag). Returns the row and whether the eye was clicked.
fn entry_row(ui: &mut egui::Ui, theme: &SerpantinumTheme, (icon, label): (&str, &str), hidden: bool, spot: Spot, lifted: bool, indent: f32) -> (egui::Rect, bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::hover());
    let (body, eye_rect) = rect.split_left_right_at_x(rect.right() - EYE_W);
    let id = ui.make_persistent_id(("cm_row", spot.group, spot.index, spot.leaf));
    let body_resp = ui.interact(body, id, egui::Sense::drag());
    let eye_resp = ui.interact(eye_rect, id.with("eye"), egui::Sense::click());
    if body_resp.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), Dragged(spot));
    }
    let painter = ui.painter();
    if body_resp.hovered() || lifted {
        painter.rect_filled(rect, egui::CornerRadius::same(material::radius_md()), theme.surface1);
    }
    let fade = if hidden || lifted { 0.45 } else { 1.0 };
    let x = rect.left() + indent;
    material::paint_icon(painter, egui::pos2(x + 14.0, rect.center().y), icons::DRAG, 16.0, theme.subtext0.gamma_multiply(fade));
    material::paint_icon(painter, egui::pos2(x + 38.0, rect.center().y), icon, 18.0, theme.text.gamma_multiply(fade));
    painter.text(egui::pos2(x + 56.0, rect.center().y), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(material::FONT_BODY), theme.text.gamma_multiply(fade));
    let eye = if hidden { icons::VISIBILITY_OFF } else { icons::VISIBILITY };
    material::paint_icon(painter, eye_rect.center(), eye, 18.0, if eye_resp.hovered() { theme.mauve } else { theme.subtext0 });
    if body_resp.dragged() || lifted {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    (rect, eye_resp.clicked())
}

/// The id under which the name field of the submenu at `spot` keeps what is typed into it.
fn name_field(spot: Spot) -> egui::Id {
    egui::Id::unique("cm_submenu_name").with((spot.group, spot.index))
}

/// A row of a submenu: its name can be typed over, its handle dragged, its eye clicked.
fn submenu_row(ui: &mut egui::Ui, theme: &SerpantinumTheme, label: &str, hidden: bool, spot: Spot, lifted: bool) -> (egui::Rect, bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::hover());
    let (handle, rest) = rect.split_left_right_at_x(rect.left() + 48.0);
    let (field, eye_rect) = rest.split_left_right_at_x(rest.right() - EYE_W);
    let id = ui.make_persistent_id(("cm_row", spot.group, spot.index, spot.leaf));
    let handle_resp = ui.interact(handle, id, egui::Sense::drag());
    let eye_resp = ui.interact(eye_rect, id.with("eye"), egui::Sense::click());
    if handle_resp.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), Dragged(spot));
    }
    let fade = if hidden || lifted { 0.45 } else { 1.0 };
    ui.painter().rect_filled(rect, egui::CornerRadius::same(material::radius_md()), theme.surface1.gamma_multiply(fade));
    material::paint_icon(ui.painter(), egui::pos2(rect.left() + 14.0, rect.center().y), icons::DRAG, 16.0, theme.subtext0.gamma_multiply(fade));
    material::paint_icon(ui.painter(), egui::pos2(rect.left() + 38.0, rect.center().y), icons::FOLDER, 18.0, theme.text.gamma_multiply(fade));
    let key = name_field(spot);
    let mut text = ui.ctx().data(|d| d.get_temp::<String>(key)).unwrap_or_else(|| label.to_owned());
    let edit = material::put_over(ui, field.shrink2(egui::vec2(8.0, 4.0)), egui::TextEdit::singleline(&mut text).hint_text(tr("Submenu name")).font(egui::FontId::proportional(material::FONT_BODY)));
    if edit.changed() {
        ui.ctx().data_mut(|d| d.insert_temp(key, text.clone()));
    }
    if edit.lost_focus() {
        ui.ctx().data_mut(|d| d.insert_temp(key.with("done"), text));
    }
    let eye = if hidden { icons::VISIBILITY_OFF } else { icons::VISIBILITY };
    material::paint_icon(ui.painter(), eye_rect.center(), eye, 18.0, if eye_resp.hovered() { theme.mauve } else { theme.subtext0 });
    (rect, eye_resp.clicked())
}

/// Takes in the names typed into submenu fields once the field was left: an empty name keeps the old one.
fn take_renames(ctx: &egui::Context, layout: &mut MenuLayout) -> bool {
    let mut changed = false;
    for (g, group) in layout.groups.iter_mut().enumerate() {
        for (i, node) in group.iter_mut().enumerate() {
            let key = name_field(Spot { group: g, index: i, leaf: None });
            let Some(done) = ctx.data_mut(|d| d.remove_temp::<String>(key.with("done"))) else { continue };
            ctx.data_mut(|d| d.remove::<String>(key));
            if let MenuNode::Submenu { name, .. } = node {
                let typed = done.trim();
                if !typed.is_empty() && typed != name {
                    *name = typed.to_owned();
                    changed = true;
                }
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(groups: &[Vec<MenuEntry>]) -> Vec<Vec<String>> {
        groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|(_, label, action)| match action {
                        MenuAction::Submenu(children) => format!("{label} > {}", children.iter().map(|c| c.1).collect::<Vec<_>>().join(", ")),
                        _ => label.to_string(),
                    })
                    .collect()
            })
            .collect()
    }

    fn made_folder_menu() -> Vec<Vec<MenuEntry>> {
        let item = FileItem::from_path(Path::new("/")).unwrap();
        let p = item.path.clone();
        vec![
            vec![
                (icons::OPEN, tr("Open"), MenuAction::Open(item)),
                (icons::NEW_TAB, tr("Open in"), MenuAction::Submenu(vec![
                    (icons::NEW_TAB, tr("New tab"), MenuAction::OpenInNewTab(p.clone())),
                    (icons::NEW_WINDOW, tr("New window"), MenuAction::OpenInNewWindow(p.clone())),
                ])),
            ],
            vec![(icons::COPY, tr("Copy"), MenuAction::Copy(vec![p.clone()])), (icons::CUT, tr("Cut"), MenuAction::Cut(vec![p.clone()]))],
            vec![(icons::TAG, "Tags", MenuAction::Submenu(vec![
                (icons::TAG, "work", MenuAction::Plugin("tags", "toggle:work".into(), vec![p.clone()], None)),
                (icons::ADD, tr("New tag…"), MenuAction::Plugin("tags", "new".into(), vec![p.clone()], None)),
            ]))],
            vec![(icons::INFO, tr("Properties"), MenuAction::Properties(p))],
        ]
    }

    fn strings(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
    }

    #[test]
    fn a_layout_puts_entries_where_it_says_hides_what_it_hides_and_keeps_what_it_does_not_know() {
        let layout = MenuLayout {
            groups: vec![
                vec![entry("properties"), entry("copy")],
                vec![MenuNode::Entry { id: "cut".into(), hidden: true }, submenu("My things", &["show_in_folder", "new_window"])],
            ],
        };
        let out = apply(made_folder_menu(), &layout, &[]);
        assert_eq!(
            labels(&out),
            strings(&[
                &["Properties", "Copy"],
                // Not named by the layout: as it came, before the last group; "New tab" left its submenu and says all of it
                &["Open", "Open in new tab"],
                &["Tags > work, New tag…"],
                // "Cut" is hidden; the submenu kept one entry, which stands alone under its whole name
                &["Open in new window"],
            ])
        );
    }

    #[test]
    fn a_submenu_of_the_person_gets_its_name_and_a_family_of_entries_goes_where_its_id_says() {
        let layout = MenuLayout {
            groups: vec![
                vec![submenu("Mine", &["copy", "cut", "plugin:tags:toggle:*"])],
                vec![submenu("plugin-group:tags", &["plugin:tags:new", "new_tab", "new_window"])],
                vec![entry("open"), entry("properties")],
            ],
        };
        let out = apply(made_folder_menu(), &layout, &[("tags".to_owned(), "Tags")]);
        assert_eq!(labels(&out), strings(&[&["Mine > Copy, Cut, work"], &["Tags > New tag…, New tab, New window"], &["Open", "Properties"]]));
    }

    #[test]
    fn a_lone_entry_of_a_plugin_or_own_submenu_takes_the_submenu_name_in_front() {
        let p = PathBuf::from("/x/a.rs");
        let git = |id: &str, label: &'static str| (icons::ADD, label, MenuAction::Plugin("git", id.into(), vec![p.clone()], None));
        let made = vec![
            vec![(icons::COPY, tr("Copy"), MenuAction::Copy(vec![p.clone()])), (icons::DUPLICATE, tr("Duplicate"), MenuAction::Duplicate(vec![p.clone()]))],
            vec![(icons::ADD, "Git", MenuAction::Submenu(vec![git("stage", "Stage"), git("diff", "Show diff")]))],
            vec![(icons::INFO, tr("Properties"), MenuAction::Properties(p.clone()))],
        ];
        let layout = MenuLayout {
            groups: vec![
                vec![submenu("plugin-group:git", &["plugin:git:stage"]), submenu("Mine", &["copy"]), submenu("builtin:more", &["duplicate"])],
                vec![entry("properties")],
            ],
        };
        let out = apply(made.clone(), &layout, &[("git".to_owned(), "Git")]);
        // A plugin's submenu and one of the person's: their name in front; "More actions" says all already
        assert_eq!(labels(&out)[0], ["Git: Stage", "Mine: Copy", "Duplicate"]);
        // What the layout does not name and left a plugin's submenu: the same
        assert_eq!(labels(&out)[1], ["Git: Show diff"]);
    }

    #[test]
    fn hidden_submenus_take_their_entries_with_them_and_empty_groups_go() {
        let layout = MenuLayout {
            groups: vec![
                vec![MenuNode::Submenu { name: "builtin:open_in".into(), hidden: true, entries: vec![MenuLeaf { id: "new_tab".into(), hidden: false }, MenuLeaf { id: "new_window".into(), hidden: false }] }],
                vec![],
                vec![entry("open"), entry("copy"), entry("cut"), entry("plugin:tags:toggle:*"), entry("plugin:tags:new"), entry("properties")],
            ],
        };
        let out = apply(made_folder_menu(), &layout, &[]);
        assert_eq!(labels(&out), strings(&[&["Open", "Copy", "Cut", "work", "New tag…", "Properties"]]));
    }

    fn ids(layout: &MenuLayout) -> Vec<Vec<String>> {
        layout
            .groups
            .iter()
            .map(|g| {
                g.iter()
                    .map(|n| match n {
                        MenuNode::Entry { id, hidden } => format!("{id}{}", if *hidden { "!" } else { "" }),
                        MenuNode::Submenu { name, entries, .. } => format!("{name}[{}]", entries.iter().map(|l| l.id.as_str()).collect::<Vec<_>>().join(",")),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn entries_move_between_groups_into_and_out_of_submenus_and_submenus_only_between_groups() {
        let start = || MenuLayout { groups: vec![vec![entry("a"), entry("b"), submenu("S", &["x", "y"])], vec![entry("c")]] };
        let spot = |group, index, leaf| Spot { group, index, leaf };
        let mut l = start();
        // Down within its group (the place after the one taken out moves up)
        move_node(&mut l, spot(0, 0, None), Drop::Before(spot(0, 2, None)));
        assert_eq!(ids(&l), strings(&[&["b", "a", "S[x,y]"], &["c"]]));
        // Into another group, at its end
        let mut l = start();
        move_node(&mut l, spot(0, 1, None), Drop::Before(spot(1, 9, None)));
        assert_eq!(ids(&l), strings(&[&["a", "S[x,y]"], &["c", "b"]]));
        // Into a submenu, and into it at a place
        let mut l = start();
        move_node(&mut l, spot(1, 0, None), Drop::Into { group: 0, index: 2 });
        assert_eq!(ids(&l), strings(&[&["a", "b", "S[x,y,c]"], &[]]));
        let mut l = start();
        move_node(&mut l, spot(0, 0, None), Drop::Before(spot(0, 2, Some(1))));
        assert_eq!(ids(&l), strings(&[&["b", "S[x,a,y]"], &["c"]]));
        // Out of a submenu
        let mut l = start();
        move_node(&mut l, spot(0, 2, Some(0)), Drop::Before(spot(1, 0, None)));
        assert_eq!(ids(&l), strings(&[&["a", "b", "S[y]"], &["x", "c"]]));
        // Within a submenu
        let mut l = start();
        move_node(&mut l, spot(0, 2, Some(0)), Drop::Before(spot(0, 2, Some(2))));
        assert_eq!(ids(&l), strings(&[&["a", "b", "S[y,x]"], &["c"]]));
        // A submenu goes between groups with its entries, not into a submenu
        let mut l = start();
        move_node(&mut l, spot(0, 2, None), Drop::Before(spot(1, 0, None)));
        assert_eq!(ids(&l), strings(&[&["a", "b"], &["S[x,y]", "c"]]));
        let mut l = start();
        move_node(&mut l, spot(0, 2, None), Drop::Into { group: 0, index: 2 });
        assert_eq!(ids(&l), ids(&start()), "nothing for a drop that makes no sense");
        // A drop into something that is not a submenu changes nothing either; the hidden mark goes along
        let mut l = start();
        move_node(&mut l, spot(0, 0, None), Drop::Into { group: 1, index: 0 });
        assert_eq!(ids(&l), ids(&start()));
        let mut l = MenuLayout { groups: vec![vec![MenuNode::Entry { id: "a".into(), hidden: true }], vec![]] };
        move_node(&mut l, spot(0, 0, None), Drop::Before(spot(1, 0, None)));
        assert_eq!(ids(&l), strings(&[&[], &["a!"]]));
    }

    #[test]
    fn every_entry_of_the_real_menus_has_an_id_the_settings_know() {
        // What a menu can hold, by kind, as the defaults list it (plugins left out: they bring their own ids)
        for kind in MenuKind::ALL {
            let layout = default_layout(kind, &[]);
            for id in named(&layout) {
                assert!(builtin_entry(id).is_some(), "{kind:?}: {id} has no name");
            }
        }
        assert_eq!(plugin_entry_id("tags", "toggle:work"), "plugin:tags:toggle:*");
        assert_eq!(plugin_entry_id("git", "stage"), "plugin:git:stage");
        assert_eq!(plugin_entry_id("actions", "convert.json"), "plugin:actions:convert.json");
    }

    // --- The settings page, run headless with synthetic input

    struct Page {
        app: CompressionApp,
        ctx: egui::Context,
        time: f64,
        shown: Vec<(String, egui::Rect)>,
    }

    fn texts(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.galley.rect.translate(t.pos.to_vec2()))),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut found));
        found
    }

    impl Page {
        fn new(name: &str, plugins_on: bool) -> (Self, PathBuf) {
            crate::settings::NO_SAVE.with(|n| n.set(true));
            let dir = std::env::temp_dir().join(format!("compression_menu_page_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let ctx = egui::Context::default();
            let mut settings = Settings::default();
            for id in ["git", "checksum", "duplicates", "compare", "actions", "quick_look", "tags"] {
                settings.plugins.insert(id.to_owned(), plugins_on);
            }
            let app = CompressionApp::new(dir.clone(), SerpantinumTheme::default(), settings, &ctx);
            let mut page = Page { app, ctx, time: 0.0, shown: Vec::new() };
            page.run(Vec::new(), 4);
            (page, dir)
        }

        fn run(&mut self, events: Vec<egui::Event>, frames: usize) {
            for n in 0..frames {
                self.time += 0.05;
                let raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 2400.0))),
                    time: Some(self.time),
                    events: if n == 0 { events.clone() } else { Vec::new() },
                    ..Default::default()
                };
                let app = &mut self.app;
                let mut out = self.ctx.run_ui(raw, |ui| app.context_menu_tab(ui));
                out.textures_delta.clear();
                self.shown = texts(&out);
            }
        }

        fn find(&self, text: &str) -> egui::Rect {
            self.shown.iter().find(|(t, _)| t == text).unwrap_or_else(|| panic!("{text:?} not on the page: {:?}", self.shown.iter().map(|s| &s.0).collect::<Vec<_>>())).1
        }

        fn shows(&self, text: &str) -> bool {
            self.shown.iter().any(|(t, _)| t == text)
        }

        fn press(&mut self, pos: egui::Pos2, pressed: bool) {
            self.run(vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE }], 2);
        }

        fn click(&mut self, pos: egui::Pos2) {
            self.run(vec![egui::Event::PointerMoved(pos)], 2);
            self.press(pos, true);
            self.press(pos, false);
        }

        /// Presses at `from`, moves to `to` in steps, lets go there.
        fn drag(&mut self, from: egui::Pos2, to: egui::Pos2) {
            self.run(vec![egui::Event::PointerMoved(from)], 2);
            self.press(from, true);
            for step in 1..=8 {
                let at = from + (to - from) * (step as f32 / 8.0);
                self.run(vec![egui::Event::PointerMoved(at)], 1);
            }
            self.press(to, false);
            self.run(Vec::new(), 2);
        }

        /// The eye of the row of `text`.
        fn eye_of(&self, text: &str) -> egui::Pos2 {
            let row = self.find(text);
            egui::pos2(800.0 - 12.0 - EYE_W / 2.0, row.center().y)
        }

        fn layout(&self, kind: MenuKind) -> Option<MenuLayout> {
            self.app.menu_layout(kind).cloned()
        }
    }

    fn ids_of(layout: &MenuLayout) -> Vec<Vec<String>> {
        ids(layout)
    }

    #[test]
    fn the_page_has_a_tab_for_each_menu_with_its_entries_and_those_of_the_plugins_that_are_on() {
        let (mut page, dir) = Page::new("tabs", true);
        for name in ["Files", "Folders", "Empty space"] {
            assert!(page.shows(name), "{name}");
        }
        for entry in ["Open", "Open with…", "Copy", "Cut", "More actions", "Properties", "Git", "Stage", "Tags", "Each tag", "Verify files listed"] {
            assert!(page.shows(entry), "Files: {entry}");
        }
        page.click(page.find("Folders").center());
        assert!(page.shows("Open in") && page.shows("New tab") && page.shows("Custom icon") && page.shows("Calculate size"));
        page.click(page.find("Empty space").center());
        assert!(page.shows("New file") && page.shows("Select") && page.shows("Find duplicate files…") && page.shows("Compare folders…"));
        // A plugin that is off: its entries are not offered
        let (page, dir2) = Page::new("tabs_off", false);
        assert!(page.shows("Copy") && !page.shows("Git") && !page.shows("Verify files listed"));
        assert!(page.layout(MenuKind::File).is_none(), "looking changes nothing");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn the_eye_hides_an_entry_without_starting_a_drag_and_the_menu_follows() {
        let (mut page, dir) = Page::new("eye", false);
        let eye = page.eye_of("Cut");
        // Pressed and moved a little on the eye: no drag, only the click
        page.run(vec![egui::Event::PointerMoved(eye)], 2);
        page.press(eye, true);
        page.run(vec![egui::Event::PointerMoved(eye + egui::vec2(0.0, 30.0))], 2);
        assert!(egui::DragAndDrop::payload::<Dragged>(&page.ctx).is_none(), "the eye started a drag");
        page.run(vec![egui::Event::PointerMoved(eye)], 1);
        page.press(eye, false);
        assert!(page.layout(MenuKind::File).is_none_or(|l| l == default_layout(MenuKind::File, &[])), "a press that wandered off is no click and no drag");
        // A click on the eye hides the entry
        page.click(eye);
        let layout = page.layout(MenuKind::File).expect("the layout was kept");
        assert!(layout.groups.iter().flatten().any(|n| matches!(n, MenuNode::Entry { id, hidden: true } if id == "cut")), "{:?}", ids_of(&layout));
        // The menu of a file has no "Cut" now, and still has "Copy"
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let item = FileItem::from_path(&dir.join("a.txt")).unwrap();
        let menu: Vec<&str> = page.app.menu_entries(&MenuTarget::Item(item)).iter().flatten().map(|e| e.1).collect();
        assert!(menu.contains(&"Copy") && !menu.contains(&"Cut"), "{menu:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entries_are_dragged_between_groups_and_into_a_submenu_and_reset_sits_beside_new_group() {
        let (mut page, dir) = Page::new("drag", false);
        // "Properties" to the top of the first group, onto the row of "Open"
        let (props, open) = (page.find("Properties"), page.find("Open"));
        page.drag(props.left_center() + egui::vec2(-20.0, 0.0), egui::pos2(300.0, open.top() + 4.0));
        let layout = page.layout(MenuKind::File).expect("kept");
        assert_eq!(ids_of(&layout)[0][0], "properties", "{:?}", ids_of(&layout));
        // "Cut" into the submenu "More actions" (the lower part of its row)
        let (cut, more) = (page.find("Cut"), page.find("More actions"));
        page.drag(cut.left_center() + egui::vec2(-20.0, 0.0), egui::pos2(300.0, more.bottom() + 2.0));
        let layout = page.layout(MenuKind::File).unwrap();
        let more_entries = layout.groups.iter().flatten().find_map(|n| match n {
            MenuNode::Submenu { name, entries, .. } if name == "builtin:more" => Some(entries.iter().map(|l| l.id.clone()).collect::<Vec<_>>()),
            _ => None,
        });
        assert!(more_entries.is_some_and(|e| e.contains(&"cut".to_owned())), "{:?}", ids_of(&layout));
        // Reset is beside New group, on its row, and takes the layout back to the built-in one
        let (new_group, reset) = (page.find("New group"), page.find("Reset"));
        assert!((reset.center().y - new_group.center().y).abs() < 2.0 && reset.left() > new_group.right() && reset.left() - new_group.right() < 80.0, "{new_group:?} {reset:?}");
        page.click(reset.center());
        assert!(page.layout(MenuKind::File).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_submenu_takes_the_name_typed_for_it() {
        let (mut page, dir) = Page::new("submenu", false);
        page.click(page.find("New submenu").center());
        // The name field of the new submenu sits above the buttons, which carry the same words
        let field = page.shown.iter().filter(|(t, _)| t == "New submenu").map(|(_, r)| *r).min_by(|a, b| a.top().total_cmp(&b.top())).expect("the new submenu's name");
        page.click(field.center());
        page.run(vec![egui::Event::Key { key: egui::Key::A, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND }], 1);
        page.run(vec![egui::Event::Text("Tools".into())], 2);
        page.run(vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }], 3);
        let layout = page.layout(MenuKind::File).unwrap();
        assert!(layout.groups.iter().flatten().any(|n| matches!(n, MenuNode::Submenu { name, .. } if name == "Tools")), "{:?}", ids_of(&layout));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
