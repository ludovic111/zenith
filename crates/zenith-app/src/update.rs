//! Updates in the window: a check when the app starts (unless turned off in Settings or with
//! `ZENITH_NO_UPDATE=1`), "Check for Updates…" in the menu and the palette, and a one-click
//! install that swaps the signed app and relaunches it (`zenith_commands::update`).

use gpui::{App, AppContext, Context, Entity, Global};
use zenith_commands::update::{self, Release};

use crate::store;

#[derive(Clone, Debug, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Available(Release),
    Installing(Release),
    UpToDate,
    Failed(String),
}

pub struct Updates {
    pub state: UpdateState,
}

struct GlobalUpdates(Entity<Updates>);

impl Global for GlobalUpdates {}

pub fn updates(cx: &mut App) -> Entity<Updates> {
    if let Some(global) = cx.try_global::<GlobalUpdates>() {
        return global.0.clone();
    }
    let entity = cx.new(|_| Updates { state: UpdateState::Idle });
    cx.set_global(GlobalUpdates(entity.clone()));
    entity
}

fn check(announce: bool, cx: &mut App) {
    let entity = updates(cx);
    if matches!(entity.read(cx).state, UpdateState::Checking | UpdateState::Installing(_)) {
        return;
    }
    entity.update(cx, |u, cx| {
        u.state = UpdateState::Checking;
        cx.notify();
    });
    let task = crate::runtime::spawn(update::check());
    cx.spawn(async move |cx| {
        let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
        let _ = cx.update(|cx| {
            let state = match result {
                Ok(Some(release)) => {
                    let message = format!("zenith {} is available. Settings › Updates installs it.", release.version);
                    store::store(cx).update(cx, |s, cx| s.notify_info(message, cx));
                    UpdateState::Available(release)
                }
                Ok(None) => {
                    if announce {
                        let message = if update::disabled() {
                            "Updates are off (ZENITH_NO_UPDATE).".to_owned()
                        } else {
                            format!("zenith {} is the latest.", update::current_version())
                        };
                        store::store(cx).update(cx, |s, cx| s.notify_info(message, cx));
                    }
                    UpdateState::UpToDate
                }
                Err(error) => {
                    if announce {
                        let message = format!("Could not check for updates: {error}");
                        store::store(cx).update(cx, |s, cx| s.notify_error(message, cx));
                    }
                    UpdateState::Failed(error)
                }
            };
            updates(cx).update(cx, |u, cx| {
                u.state = state;
                cx.notify();
            });
        });
    })
    .detach();
}

pub fn check_on_start(cx: &mut App) {
    std::thread::spawn(update::forget_previous);
    if crate::prefs::Prefs::load().check_for_updates && !update::disabled() {
        check(false, cx);
    }
}

pub fn check_now(cx: &mut App) {
    check(true, cx);
}

/// Installs the available release, then relaunches zenith on it.
pub fn install<T: 'static>(cx: &mut Context<T>) {
    let entity = updates(cx);
    let UpdateState::Available(release) = entity.read(cx).state.clone() else {
        return;
    };
    entity.update(cx, |u, cx| {
        u.state = UpdateState::Installing(release.clone());
        cx.notify();
    });
    let task = crate::runtime::spawn(async move { update::install(&release).await.map(|path| (path, release)) });
    cx.spawn(async move |_, cx| {
        let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
        let _ = cx.update(|cx| match result {
            Ok((path, _)) => {
                // Relaunch from the new copy once this one has quit.
                let _ = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(format!("sleep 1; /usr/bin/open {:?}", path.display().to_string()))
                    .spawn();
                cx.quit();
            }
            Err(error) => {
                let message = format!("The update did not install: {error}");
                store::store(cx).update(cx, |s, cx| s.notify_error(message, cx));
                updates(cx).update(cx, |u, cx| {
                    u.state = UpdateState::Failed(error);
                    cx.notify();
                });
            }
        });
    })
    .detach();
}
