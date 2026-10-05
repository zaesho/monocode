//! One project, many machines (docs/repo-machines.md): machine labels, a
//! project's locations, automatic linking by repository, and the actions
//! that add a location on this computer or move a blank session to another
//! machine.

use std::collections::HashSet;

use gpui::{App, AppContext as _, Global, Subscription};
use monocode_engine::projects::project_machines::{LOCAL_MACHINE, location_machine};
use monocode_engine::projects::{MachineNames, ProjectsEvent, ProjectsGlobal, actions};
use monocode_engine::remote::RemoteGlobal;
use monocode_engine::remote::remote_connections::RemoteEvent;
use monocode_engine::remote::remote_projects::{self, remote_project_for};
use monocode_engine::runtime::Engine;
use monocode_layout::paths::is_remote_project_path;
use monocode_layout::project_return::is_blank_session;
use monocode_remote::host::protocol::HostProject;

/// The label of this computer in machine lists.
pub fn this_machine_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "This Mac"
    } else if cfg!(windows) {
        "This PC"
    } else {
        "This computer"
    }
}

/// Paired machine names by environment id.
pub fn machine_names(cx: &App) -> MachineNames {
    RemoteGlobal::try_global(cx)
        .map(|remote| {
            remote
                .connections
                .read(cx)
                .machines()
                .iter()
                .map(|machine| (machine.environment_id.clone(), machine.name.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether any machine is paired.
pub fn has_paired_machine(cx: &App) -> bool {
    RemoteGlobal::try_global(cx)
        .is_some_and(|remote| !remote.connections.read(cx).machines().is_empty())
}

/// The machine a location is on, as the user reads it.
pub fn machine_label(path: &str, names: &MachineNames) -> String {
    let machine = location_machine(path);
    if machine == LOCAL_MACHINE {
        return this_machine_label().into();
    }
    names
        .get(&machine)
        .cloned()
        .unwrap_or_else(|| "Remote machine".into())
}

/// One folder of a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    /// "This Mac" or the machine's name.
    pub machine: String,
    pub remote: bool,
}

/// `projectLocations` with machine labels: the home first, then the local
/// member, then remote members by machine name.
pub fn project_locations(path: &str, cx: &App) -> Vec<Location> {
    let Some(global) = ProjectsGlobal::try_global(cx) else {
        return Vec::new();
    };
    let names = machine_names(cx);
    global
        .projects
        .read(cx)
        .project_locations(path, &names)
        .into_iter()
        .map(|path| Location {
            machine: machine_label(&path, &names),
            remote: is_remote_project_path(&path),
            path,
        })
        .collect()
}

/// Whether a session may still move to another location: it exists, and no
/// message was sent, here or on a host.
pub fn session_can_move(session_id: &str, cx: &App) -> bool {
    let Some(engine) = Engine::try_global(cx) else {
        return false;
    };
    let sessions = engine.sessions.read(cx);
    let Some(session) = sessions.get(session_id) else {
        return false;
    };
    is_blank_session(Some(session))
        && (!is_remote_project_path(&session.cwd)
            || RemoteGlobal::remote_session_for(session_id, cx).is_none())
}

/// Move a blank session to another location of its project through the
/// composer's folder retarget. Does nothing once it has a message.
pub fn move_blank_session(session_id: &str, path: &str, cx: &mut App) {
    if session_can_move(session_id, cx) {
        actions::on_cwd_change(session_id, path, cx);
    }
}

/// "Add folder on this computer…": pick a folder, link it to the project,
/// and move the blank session that asked to it.
pub fn add_local_location(home: String, session_id: Option<String>, cx: &mut App) {
    let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Add folder".into()),
    });
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = picked.await else {
            return;
        };
        let Some(path) = paths.first() else {
            return;
        };
        let path = monocode_platform::path_to_js(path);
        cx.update(|cx| link_location(&home, &path, session_id.as_deref(), cx));
    })
    .detach();
}

/// Link a new location to `home`'s project, then move the blank session that
/// asked for it there. Says why when the project already has a folder on
/// that machine.
pub fn link_location(home: &str, path: &str, session_id: Option<&str>, cx: &mut App) {
    let Some(global) = ProjectsGlobal::try_global(cx) else {
        return;
    };
    let projects = global.projects.clone();
    let linked = projects.update(cx, |projects, cx| projects.link_location(home, path, cx));
    if !linked {
        let names = machine_names(cx);
        monocode_app::bridge::dialogs::alert(
            &format!(
                "This project already has a folder on {}.",
                machine_label(path, &names)
            ),
            true,
            cx,
        );
        return;
    }
    if let Some(session_id) = session_id {
        move_blank_session(session_id, path, cx);
    }
    auto_link_projects(cx);
}

/// `unlinkProjectLocation`.
pub fn unlink_location(path: &str, cx: &mut App) {
    let Some(global) = ProjectsGlobal::try_global(cx) else {
        return;
    };
    let projects = global.projects.clone();
    let names = machine_names(cx);
    projects.update(cx, |projects, cx| {
        projects.unlink_location(path, &names, cx)
    });
}

#[derive(Default)]
struct AutoLink {
    running: bool,
    again: bool,
    _subscriptions: Vec<Subscription>,
}

impl Global for AutoLink {}

/// Link rail projects of one repository after the rail loads, when a remote
/// project is added, and when a machine lists its projects.
pub fn init(cx: &mut App) {
    if cx.has_global::<AutoLink>() {
        return;
    }
    let Some(projects) = ProjectsGlobal::try_global(cx).map(|global| global.projects.clone())
    else {
        return;
    };
    let mut subscriptions = vec![cx.subscribe(&projects, |_, event, cx| {
        if *event == ProjectsEvent::PathsChanged {
            auto_link_projects(cx);
        }
    })];
    if let Some(remote) = RemoteGlobal::try_global(cx) {
        let connections = remote.connections.clone();
        subscriptions.push(cx.subscribe(&connections, |_, event, cx| match event {
            RemoteEvent::MachinesChanged => list_machine_projects(cx),
            RemoteEvent::ProjectsChanged => auto_link_projects(cx),
            _ => {}
        }));
    }
    cx.set_global(AutoLink {
        _subscriptions: subscriptions,
        ..Default::default()
    });
    auto_link_projects(cx);
    list_machine_projects(cx);
}

/// Ask each paired machine with a rail project for `projects.list`, and
/// keep the `remoteUrl` each project reports.
fn list_machine_projects(cx: &mut App) {
    let Some(remote) = RemoteGlobal::try_global(cx) else {
        return;
    };
    let connections = remote.connections.clone();
    let kv = remote.kv.clone();
    let machines: Vec<_> = connections
        .read(cx)
        .machines()
        .iter()
        .filter(|machine| {
            !remote_projects::remote_projects_on(&kv, &machine.environment_id).is_empty()
        })
        .cloned()
        .collect();
    for machine in machines {
        let request =
            connections
                .read(cx)
                .request(&machine.id, "projects.list", serde_json::json!({}));
        let kv = kv.clone();
        cx.spawn(async move |cx| {
            let Ok(value) = request.await else {
                return;
            };
            let Ok(listed) = serde_json::from_value::<Vec<HostProject>>(value) else {
                return;
            };
            let mut identities = Vec::new();
            for project in listed {
                let Some(url) = project.remote_url else {
                    continue;
                };
                let key =
                    remote_projects::remote_project_key(&machine.environment_id, &project.cwd);
                if remote_projects::set_remote_project_url(&kv, &key, &url)
                    || remote_project_for(&kv, &key).is_some()
                {
                    identities.push((key, url));
                }
            }
            cx.update(|cx| remember_and_link(identities, cx));
        })
        .detach();
    }
}

fn remember_and_link(identities: Vec<(String, String)>, cx: &mut App) {
    let Some(projects) = ProjectsGlobal::try_global(cx).map(|global| global.projects.clone())
    else {
        return;
    };
    let names = machine_names(cx);
    projects.update(cx, |projects, cx| {
        projects.remember_identities(&identities, cx);
        projects.apply_auto_links(&names, cx);
    });
}

/// `autoLinkProjects`: fill missing identities (git on this computer, the
/// saved `remoteUrl` for a remote folder), then link rail projects of one
/// repository on different machines.
pub fn auto_link_projects(cx: &mut App) {
    let Some(global) = ProjectsGlobal::try_global(cx) else {
        return;
    };
    let (projects, backend) = (global.projects.clone(), global.backend.clone());
    {
        let Some(state) = cx.try_global::<AutoLink>() else {
            return;
        };
        if state.running {
            cx.global_mut::<AutoLink>().again = true;
            return;
        }
    }
    let (rail, machines) = {
        let projects = projects.read(cx);
        (
            projects.machines().rail_projects(projects.recents()),
            projects.machines().clone(),
        )
    };
    let kv = RemoteGlobal::try_global(cx).map(|remote| remote.kv.clone());
    let mut known = Vec::new();
    let mut local = Vec::new();
    for item in rail {
        if machines.identity(&item.path).is_some() {
            continue;
        }
        if is_remote_project_path(&item.path) {
            if let Some(url) = kv
                .as_ref()
                .and_then(|kv| remote_project_for(kv, &item.path))
                .and_then(|record| record.remote_url)
            {
                known.push((item.path, url));
            }
        } else {
            local.push(item.path);
        }
    }
    if local.is_empty() {
        remember_and_link(known, cx);
        return;
    }
    cx.global_mut::<AutoLink>().running = true;
    let lookup = cx.background_spawn(async move {
        let mut seen = HashSet::new();
        local
            .into_iter()
            .filter(|path| seen.insert(path.clone()))
            .map(|path| {
                let url = backend.git_remote_url(&path).unwrap_or_default();
                (path, url)
            })
            .collect::<Vec<_>>()
    });
    cx.spawn(async move |cx| {
        let found = lookup.await;
        cx.update(|cx| {
            known.extend(found);
            remember_and_link(known, cx);
            let again = {
                let state = cx.global_mut::<AutoLink>();
                state.running = false;
                std::mem::take(&mut state.again)
            };
            if again {
                auto_link_projects(cx);
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_this_computer_and_named_machines() {
        let names: MachineNames = [("mini".to_string(), "Mini".to_string())].into();
        assert_eq!(machine_label("/work/app", &names), this_machine_label());
        assert_eq!(machine_label("remote://mini/app", &names), "Mini");
        assert_eq!(machine_label("remote://gone/app", &names), "Remote machine");
    }
}
