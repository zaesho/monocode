//! One project, many machines (docs/repo-machines.md): a rail project is a
//! repository with at most one folder ("location") per machine. The first
//! location is the home, which keeps the project's rail slot and settings;
//! the others are members linked to it. Saved under
//! `monocode.projectMachines.v1` with the same JSON as the TypeScript app.

use std::collections::{HashMap, HashSet};

use monocode_core::paths::path_key;
use monocode_layout::paths::parse_remote_path;
use monocode_layout::tab_groups::JsRecord;
use monocode_settings::Kv;
use serde::Serialize;
use serde_json::Value;

use super::js_object::parse_object;
use super::recents::{
    RecentProject, is_remote_project_path, looks_like_project, normalize_project_path,
    same_project_path,
};

pub use monocode_core::git_remote::normalize_git_remote_url;

pub const KEY: &str = "monocode.projectMachines.v1";
/// `locationMachine` of a folder on this computer.
pub const LOCAL_MACHINE: &str = "local";

/// Machine names by environment id, for ordering members. A missing name
/// sorts by its environment id.
pub type MachineNames = HashMap<String, String>;

/// The stored links, unlinked locations, and repository identities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProjectMachines {
    /// Member path to home path. Never chains.
    pub links: JsRecord<String>,
    /// Locations the user unlinked. Automatic linking skips them.
    pub separate: Vec<String>,
    /// Canonical repository key per location. Empty means no remote.
    pub identities: JsRecord<String>,
}

/// `locationMachine`: `local`, or the environment id of a remote location.
pub fn location_machine(path: &str) -> String {
    if is_remote_project_path(path) {
        parse_remote_path(path)
            .map(|parts| parts.environment_id)
            .unwrap_or_default()
    } else {
        LOCAL_MACHINE.into()
    }
}

fn string_record(value: Option<&Value>) -> JsRecord<String> {
    let mut record = JsRecord::new();
    let Some(Value::Object(map)) = value else {
        return record;
    };
    for (key, value) in map {
        if let Some(value) = value.as_str()
            && !key.is_empty()
        {
            record.insert(normalize_project_path(key), value.to_string());
        }
    }
    record
}

/// Read the stored state. Anything malformed reads as empty.
pub fn load(kv: &Kv) -> ProjectMachines {
    let Some(stored) = kv.get_item(KEY).and_then(|raw| parse_object(&raw)) else {
        return ProjectMachines::default();
    };
    let mut links = JsRecord::new();
    for (member, home) in string_record(stored.get("links")).iter() {
        if !home.is_empty() && !same_project_path(member, home) {
            links.insert(member.to_string(), normalize_project_path(home));
        }
    }
    let separate = stored
        .get("separate")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(normalize_project_path)
                .collect()
        })
        .unwrap_or_default();
    ProjectMachines {
        links,
        separate,
        identities: string_record(stored.get("identities")),
    }
}

/// Write the state. Subscribers of the key reload.
pub fn save(kv: &Kv, machines: &ProjectMachines) {
    if let Ok(json) = serde_json::to_string(machines) {
        kv.set_item(KEY, &json);
    }
}

fn find<'a>(record: &'a JsRecord<String>, path: &str) -> Option<(&'a str, &'a String)> {
    let key = path_key(path);
    record.iter().find(|(entry, _)| path_key(entry) == key)
}

fn remove(record: &mut JsRecord<String>, path: &str) -> Option<String> {
    let key = find(record, path)?.0.to_string();
    record.remove(&key)
}

fn machine_sort_key(path: &str, names: &MachineNames) -> (bool, String, String) {
    let machine = location_machine(path);
    let local = machine == LOCAL_MACHINE;
    let name = names.get(&machine).cloned().unwrap_or(machine);
    (!local, name.to_lowercase(), path_key(path))
}

impl ProjectMachines {
    /// The home a member links to.
    pub fn home_of(&self, path: &str) -> Option<String> {
        find(&self.links, path).map(|(_, home)| home.clone())
    }

    /// `projectHome`: the home for a member, else the path itself.
    pub fn project_home(&self, path: &str) -> String {
        self.home_of(path)
            .unwrap_or_else(|| normalize_project_path(path))
    }

    /// Whether the path is linked to another home.
    pub fn is_member(&self, path: &str) -> bool {
        self.home_of(path).is_some()
    }

    fn members_of(&self, home: &str) -> Vec<String> {
        let key = path_key(home);
        self.links
            .iter()
            .filter(|(_, entry)| path_key(entry) == key)
            .map(|(member, _)| member.to_string())
            .collect()
    }

    /// `projectLocations`: the home first, then the members, the local one
    /// first and the remote ones by machine name.
    pub fn project_locations(&self, path: &str, names: &MachineNames) -> Vec<String> {
        let home = self.project_home(path);
        let mut members = self.members_of(&home);
        members.sort_by_key(|member| machine_sort_key(member, names));
        std::iter::once(home).chain(members).collect()
    }

    fn machines_of(&self, path: &str) -> HashSet<String> {
        let home = self.project_home(path);
        std::iter::once(home.clone())
            .chain(self.members_of(&home))
            .map(|location| location_machine(&location))
            .collect()
    }

    /// The location the project has on one machine.
    pub fn location_on(&self, path: &str, machine: &str, names: &MachineNames) -> Option<String> {
        self.project_locations(path, names)
            .into_iter()
            .find(|location| location_machine(location) == machine)
    }

    pub fn is_separate(&self, path: &str) -> bool {
        self.separate
            .iter()
            .any(|entry| same_project_path(entry, path))
    }

    fn drop_separate(&mut self, path: &str) {
        self.separate
            .retain(|entry| !same_project_path(entry, path));
    }

    fn add_separate(&mut self, path: &str) {
        if !self.is_separate(path) {
            self.separate.push(normalize_project_path(path));
        }
    }

    /// The cached repository key: `None` when unknown, empty when the
    /// location has no remote.
    pub fn identity(&self, path: &str) -> Option<&str> {
        find(&self.identities, path).map(|(_, identity)| identity.as_str())
    }

    /// Cache a location's repository key. Returns whether it changed.
    pub fn set_identity(&mut self, path: &str, identity: &str) -> bool {
        if self.identity(path) == Some(identity) {
            return false;
        }
        remove(&mut self.identities, path);
        self.identities
            .insert(normalize_project_path(path), identity.to_string());
        true
    }

    /// `linkProjectLocation`: link `member` (and any members of its own) to
    /// `home`'s project. Refuses when that would give the project two
    /// locations on one machine.
    pub fn link(&mut self, home: &str, member: &str) -> bool {
        let home = self.project_home(home);
        let member = normalize_project_path(member);
        if same_project_path(&home, &member) {
            return false;
        }
        if self
            .home_of(&member)
            .is_some_and(|current| same_project_path(&current, &home))
        {
            return true;
        }
        let taken = self.machines_of(&home);
        let moving: Vec<String> = std::iter::once(member.clone())
            .chain(if self.is_member(&member) {
                Vec::new()
            } else {
                self.members_of(&member)
            })
            .collect();
        if moving
            .iter()
            .any(|location| taken.contains(&location_machine(location)))
        {
            return false;
        }
        for location in &moving {
            remove(&mut self.links, location);
            self.links.insert(location.clone(), home.clone());
        }
        self.drop_separate(&member);
        true
    }

    /// Move a home's members under its first member, which becomes the
    /// home. Returns the new home.
    fn promote(&mut self, home: &str, names: &MachineNames) -> Option<String> {
        let locations = self.project_locations(home, names);
        let next = locations.get(1)?.clone();
        remove(&mut self.links, &next);
        for member in &locations[2..] {
            remove(&mut self.links, member);
            self.links.insert(member.clone(), next.clone());
        }
        Some(next)
    }

    /// `unlinkProjectLocation`: the location becomes its own rail project.
    /// Returns the paths the rail must remember: the location, and a
    /// promoted home.
    pub fn unlink(&mut self, location: &str, names: &MachineNames) -> Vec<String> {
        let location = normalize_project_path(location);
        let mut remember = Vec::new();
        if remove(&mut self.links, &location).is_none() {
            let Some(promoted) = self.promote(&location, names) else {
                return remember;
            };
            remember.push(promoted);
        }
        self.add_separate(&location);
        remember.push(location);
        remember
    }

    /// `forgetMachineLocation`: drop a location removed from the rail. A
    /// removed home promotes its first member, which is returned.
    pub fn forget(&mut self, path: &str, names: &MachineNames) -> Option<String> {
        let promoted = if self.is_member(path) {
            None
        } else {
            self.promote(path, names)
        };
        remove(&mut self.links, path);
        self.drop_separate(path);
        remove(&mut self.identities, path);
        promoted
    }

    /// A renamed folder keeps its links, unlink choice, and identity.
    pub fn replace_path(&mut self, from: &str, to: &str) -> bool {
        let to = normalize_project_path(to);
        let from_key = path_key(from);
        let before = self.clone();
        let mut links = JsRecord::new();
        for (member, home) in self.links.iter() {
            let rename = |path: &str| {
                if path_key(path) == from_key {
                    to.clone()
                } else {
                    path.to_string()
                }
            };
            links.insert(rename(member), rename(home));
        }
        self.links = links;
        for entry in &mut self.separate {
            if path_key(entry) == from_key {
                *entry = to.clone();
            }
        }
        if let Some(identity) = remove(&mut self.identities, from) {
            self.identities.insert(to, identity);
        }
        *self != before
    }

    /// Step 1 of `autoLinkProjects`: every recent project that is not a
    /// member, once.
    pub fn rail_projects(&self, recents: &[RecentProject]) -> Vec<RecentProject> {
        let mut seen = HashSet::new();
        recents
            .iter()
            .filter(|item| looks_like_project(&item.path) && !self.is_member(&item.path))
            .filter(|item| seen.insert(path_key(&item.path)))
            .cloned()
            .collect()
    }

    /// Steps 3 and 4 of `autoLinkProjects`: the `(home, member)` links to
    /// make. Rail projects that share a repository and live on different
    /// machines join the project with the most members (a local one wins a
    /// tie, then the one opened first). Each machine takes its most recently
    /// opened location; the rest stay separate.
    pub fn auto_link_plan(
        &self,
        recents: &[RecentProject],
        names: &MachineNames,
    ) -> Vec<(String, String)> {
        let mut groups: Vec<(String, Vec<RecentProject>)> = Vec::new();
        for item in self.rail_projects(recents) {
            if self.is_separate(&item.path) {
                continue;
            }
            let Some(identity) = self.identity(&item.path).filter(|id| !id.is_empty()) else {
                continue;
            };
            match groups.iter_mut().find(|(key, _)| key == identity) {
                Some((_, members)) => members.push(item),
                None => groups.push((identity.to_string(), vec![item])),
            }
        }
        let mut plan = Vec::new();
        for (_, mut group) in groups {
            if group.len() < 2 {
                continue;
            }
            let member_count =
                |item: &RecentProject| self.project_locations(&item.path, names).len();
            let home_index = (0..group.len())
                .max_by(|&a, &b| {
                    let (a, b) = (&group[a], &group[b]);
                    member_count(a)
                        .cmp(&member_count(b))
                        .then(is_remote_project_path(&b.path).cmp(&is_remote_project_path(&a.path)))
                        .then(b.opened_at.cmp(&a.opened_at))
                })
                .unwrap_or(0);
            let home = group.remove(home_index);
            let mut taken = self.machines_of(&home.path);
            group.sort_by_key(|item| std::cmp::Reverse(item.opened_at));
            for item in group {
                let machines = self.machines_of(&item.path);
                if machines.is_disjoint(&taken) {
                    taken.extend(machines);
                    plan.push((home.path.clone(), item.path.clone()));
                }
            }
        }
        plan
    }
}

/// `projectHome` from storage.
pub fn project_home(kv: &Kv, path: &str) -> String {
    load(kv).project_home(path)
}

/// `projectLocations` from storage.
pub fn project_locations(kv: &Kv, path: &str, names: &MachineNames) -> Vec<String> {
    load(kv).project_locations(path, names)
}

/// `linkProjectLocation`. Returns false when the project already has a
/// location on the member's machine.
pub fn link_project_location(kv: &Kv, home: &str, member: &str) -> bool {
    let mut machines = load(kv);
    let before = machines.clone();
    let linked = machines.link(home, member);
    if machines != before {
        save(kv, &machines);
    }
    linked
}

/// `unlinkProjectLocation`. The location, and a promoted home, go back on
/// the rail as recent projects.
pub fn unlink_project_location(kv: &Kv, location: &str, names: &MachineNames) {
    let mut machines = load(kv);
    let remember = machines.unlink(location, names);
    if remember.is_empty() {
        return;
    }
    save(kv, &machines);
    for path in remember {
        super::recents::remember_project(kv, &path);
    }
}

/// `forgetMachineLocation`. A promoted home takes the removed one's place
/// in the recents.
pub fn forget_machine_location(kv: &Kv, path: &str, names: &MachineNames) {
    let mut machines = load(kv);
    let before = machines.clone();
    let promoted = machines.forget(path, names);
    if machines == before {
        return;
    }
    save(kv, &machines);
    if let Some(promoted) = promoted
        && !super::recents::load_recents(kv)
            .iter()
            .any(|item| same_project_path(&item.path, &promoted))
    {
        super::recents::remember_project(kv, &promoted);
    }
}

/// Cache repository keys. Each entry is a path and a raw remote URL (empty
/// for none), normalized here.
pub fn remember_identities(kv: &Kv, entries: &[(String, String)]) -> bool {
    let mut machines = load(kv);
    let mut changed = false;
    for (path, url) in entries {
        changed |= machines.set_identity(path, &normalize_git_remote_url(url));
    }
    if changed {
        save(kv, &machines);
    }
    changed
}

/// Make the links `autoLinkProjects` plans. Returns whether any link was
/// made.
pub fn apply_auto_links(kv: &Kv, recents: &[RecentProject], names: &MachineNames) -> bool {
    let mut machines = load(kv);
    let plan = machines.auto_link_plan(recents, names);
    let mut linked = false;
    for (home, member) in plan {
        linked |= machines.link(&home, &member);
    }
    if linked {
        save(kv, &machines);
    }
    linked
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = "remote://mini/home/me/app";
    const BOX: &str = "remote://box/srv/app";

    fn names() -> MachineNames {
        [
            ("mini".to_string(), "Mini".to_string()),
            ("box".into(), "Atlas".into()),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn reads_and_writes_the_shared_json_shape() {
        let kv = Kv::in_memory();
        kv.set_item(
            KEY,
            r#"{"links":{"remote://mini/home/me/app/":"/work/app","/x":"/x","bad":1},"separate":["/work/other/",3],"identities":{"/work/app":"github.com/a/app"}}"#,
        );
        let machines = load(&kv);
        assert_eq!(machines.project_home(MINI), "/work/app");
        assert!(!machines.is_member("/x"));
        assert!(machines.is_separate("/work/other"));
        assert_eq!(machines.identity("/work/app"), Some("github.com/a/app"));
        save(&kv, &machines);
        assert_eq!(
            serde_json::from_str::<Value>(&kv.get_item(KEY).unwrap()).unwrap(),
            serde_json::json!({
                "links": { MINI: "/work/app" },
                "separate": ["/work/other"],
                "identities": { "/work/app": "github.com/a/app" },
            })
        );
        kv.set_item(KEY, "[1]");
        assert_eq!(load(&kv), ProjectMachines::default());
    }

    #[test]
    fn location_machine_is_local_or_the_environment() {
        assert_eq!(location_machine("/work/app"), "local");
        assert_eq!(location_machine(MINI), "mini");
    }

    #[test]
    fn links_one_location_per_machine_and_lists_local_first() {
        let kv = Kv::in_memory();
        assert!(link_project_location(&kv, MINI, BOX));
        assert!(link_project_location(&kv, MINI, "/work/app"));
        assert!(!link_project_location(&kv, MINI, "/work/clone"));
        assert!(!link_project_location(&kv, BOX, "remote://mini/other"));
        assert_eq!(
            project_locations(&kv, BOX, &names()),
            [MINI, "/work/app", BOX]
        );
        assert_eq!(project_home(&kv, "/work/app/"), MINI);
    }

    #[test]
    fn linking_a_home_moves_its_members_and_clears_separate() {
        let mut machines = ProjectMachines::default();
        assert!(machines.link("/work/app", MINI));
        machines.add_separate(BOX);
        assert!(machines.link(BOX, "/work/app"));
        assert_eq!(machines.project_home(MINI), BOX);
        assert_eq!(machines.project_home("/work/app"), BOX);
        let mut other = ProjectMachines::default();
        other.add_separate(MINI);
        assert!(other.link("/work/app", MINI));
        assert!(!other.is_separate(MINI));
    }

    #[test]
    fn unlinking_a_member_marks_it_separate_and_remembers_it() {
        let kv = Kv::in_memory();
        super::super::recents::remember_project(&kv, "/work/app");
        link_project_location(&kv, "/work/app", MINI);
        unlink_project_location(&kv, MINI, &names());
        let machines = load(&kv);
        assert!(!machines.is_member(MINI));
        assert!(machines.is_separate(MINI));
        let recents = super::super::recents::load_recents(&kv);
        assert_eq!(recents[0].path, MINI);
    }

    #[test]
    fn unlinking_a_home_promotes_its_first_member() {
        let mut machines = ProjectMachines::default();
        machines.link(BOX, "/work/app");
        machines.link(BOX, MINI);
        let remember = machines.unlink(BOX, &names());
        assert_eq!(remember, ["/work/app", BOX]);
        assert_eq!(machines.project_home(MINI), "/work/app");
        assert!(!machines.is_member("/work/app"));
        assert!(machines.is_separate(BOX));
        assert!(machines.unlink("/work/lonely", &names()).is_empty());
    }

    #[test]
    fn forgetting_drops_every_record_and_promotes_a_home() {
        let kv = Kv::in_memory();
        super::super::recents::remember_project(&kv, BOX);
        let mut machines = ProjectMachines::default();
        machines.link(BOX, MINI);
        machines.link(BOX, "/work/app");
        machines.set_identity(BOX, "github.com/a/app");
        machines.add_separate("/work/old");
        save(&kv, &machines);
        forget_machine_location(&kv, BOX, &names());
        let machines = load(&kv);
        assert_eq!(machines.project_home(MINI), "/work/app");
        assert_eq!(machines.identity(BOX), None);
        assert!(
            super::super::recents::load_recents(&kv)
                .iter()
                .any(|item| item.path == "/work/app")
        );
        forget_machine_location(&kv, "/work/old", &names());
        assert!(!load(&kv).is_separate("/work/old"));
        forget_machine_location(&kv, MINI, &names());
        assert!(!load(&kv).is_member(MINI));
    }

    #[test]
    fn auto_link_groups_one_location_per_machine() {
        let mut machines = ProjectMachines::default();
        for (path, identity) in [
            ("/work/app", "github.com/a/app"),
            ("/work/app-copy", "github.com/a/app"),
            (MINI, "github.com/a/app"),
            ("remote://mini/old/app", "github.com/a/app"),
            (BOX, "github.com/a/app"),
            ("/work/other", "github.com/a/other"),
            ("remote://mini/other", ""),
        ] {
            machines.set_identity(path, identity);
        }
        machines.add_separate(BOX);
        let recents = [
            RecentProject::new("/work/app", 5),
            RecentProject::new("/work/app-copy", 1),
            RecentProject::new(MINI, 9),
            RecentProject::new("remote://mini/old/app", 2),
            RecentProject::new(BOX, 8),
            RecentProject::new("/work/other", 3),
            RecentProject::new("remote://mini/other", 4),
        ];
        // The local copy opened first is the home; the newer Mini folder
        // wins its machine; the separate one is skipped.
        assert_eq!(
            machines.auto_link_plan(&recents, &names()),
            [("/work/app-copy".to_string(), MINI.to_string())]
        );
    }

    #[test]
    fn auto_link_keeps_the_home_with_the_most_members() {
        let kv = Kv::in_memory();
        let mut machines = ProjectMachines::default();
        machines.link(MINI, BOX);
        for path in [MINI, "/work/app"] {
            machines.set_identity(path, "github.com/a/app");
        }
        save(&kv, &machines);
        let recents = [
            RecentProject::new("/work/app", 1),
            RecentProject::new(MINI, 2),
            RecentProject::new(BOX, 3),
        ];
        assert!(apply_auto_links(&kv, &recents, &names()));
        assert_eq!(project_home(&kv, "/work/app"), MINI);
        assert!(!apply_auto_links(&kv, &recents, &names()));
    }

    #[test]
    fn identities_are_normalized_when_remembered() {
        let kv = Kv::in_memory();
        assert!(remember_identities(
            &kv,
            &[
                ("/work/app".into(), "git@github.com:A/App.git".into()),
                ("/work/plain".into(), String::new()),
            ]
        ));
        let machines = load(&kv);
        assert_eq!(machines.identity("/work/app"), Some("github.com/a/app"));
        assert_eq!(machines.identity("/work/plain"), Some(""));
        assert_eq!(machines.identity("/work/unknown"), None);
        assert!(!remember_identities(
            &kv,
            &[("/work/plain".into(), String::new())]
        ));
    }

    #[test]
    fn a_renamed_folder_keeps_its_links() {
        let mut machines = ProjectMachines::default();
        machines.link("/work/app", MINI);
        machines.set_identity("/work/app", "github.com/a/app");
        assert!(machines.replace_path("/work/app", "/work/app-renamed"));
        assert_eq!(machines.project_home(MINI), "/work/app-renamed");
        assert_eq!(
            machines.identity("/work/app-renamed"),
            Some("github.com/a/app")
        );
    }
}
