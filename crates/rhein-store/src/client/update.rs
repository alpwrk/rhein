//! New releases of Rhein: looked for once at startup (when `prefs.check_updates` is on) or from
//! the About page, and installed only when the person asks (see `rhein-update`). Settings and
//! data are never touched; only the program is replaced, and only where this user installed it.

use gpui::Context;
use rhein_update::{Release, Target};

use super::Client;
use super::banners::{Banner, BannerAction};

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Update {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing(Release),
    /// On disk; the next start runs it.
    Installed(Release),
    Failed(String),
}

impl Client {
    /// Whether a found release can be installed from here: signed for this system, and this copy
    /// installed where this user may replace it (not under `/usr`).
    pub fn can_install(&self, release: &Release) -> bool {
        release.download.is_some() && self.update_target.is_some()
    }

    /// Asks GitHub for the latest release. `quiet` (at startup) keeps a failure out of sight.
    pub fn check_for_updates(&mut self, quiet: bool, cx: &mut Context<Self>) {
        if matches!(self.update, Update::Checking | Update::Installing(_) | Update::Installed(_)) {
            return;
        }
        if !rhein_update::enabled() {
            self.update = Update::Failed("Development builds don't update themselves.".into());
            cx.notify();
            return;
        }
        self.update = Update::Checking;
        cx.notify();
        let (io, http) = (self.io.clone(), self.http.clone());
        cx.spawn(async move |this, cx| {
            let found = io.spawn(async move { rhein_update::latest(&http).await }).await.map_err(anyhow::Error::from).and_then(|r| r);
            this.update(cx, |this, cx| {
                this.update = match found {
                    Ok(Some(release)) => Update::Available(release),
                    Ok(None) => Update::UpToDate,
                    Err(err) => {
                        log::warn!("update check: {err:#}");
                        if quiet { Update::Idle } else { Update::Failed(format!("Couldn't look for updates: {err}")) }
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Downloads, verifies and installs the release found.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let Update::Available(release) = &self.update else { return };
        let (Some(target), true) = (self.update_target.clone(), self.can_install(release)) else { return };
        let release = release.clone();
        self.update = Update::Installing(release.clone());
        cx.notify();
        let (io, http) = (self.io.clone(), self.http.clone());
        cx.spawn(async move |this, cx| {
            let job = release.clone();
            let done = io
                .spawn(async move {
                    let tarball = rhein_update::download(&http, &job).await?;
                    tokio::task::spawn_blocking(move || rhein_update::install(&tarball, &target)).await?
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r);
            this.update(cx, |this, cx| {
                match done {
                    Ok(()) => this.update = Update::Installed(release),
                    Err(err) => {
                        log::warn!("update install: {err:#}");
                        this.update = Update::Available(release);
                        this.flash_notice(format!("Couldn't install the update: {err}"), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Starts the installed update and quits this copy.
    pub fn restart_for_update(&mut self, cx: &mut Context<Self>) {
        let Some(target) = &self.update_target else { return };
        match rhein_update::launch(target) {
            Ok(()) => cx.quit(),
            Err(err) => self.flash_notice(format!("{err:#}"), cx),
        }
    }

    pub(super) fn update_banner(&self) -> Option<Banner> {
        Some(match &self.update {
            Update::Available(r) if self.can_install(r) => Banner {
                key: format!("update:{}", r.version),
                text: format!("Rhein {} is out.", r.version),
                urgent: false,
                action: Some(("Install".into(), BannerAction::InstallUpdate)),
            },
            Update::Available(r) => Banner {
                key: format!("update:{}", r.version),
                text: match self.update_target {
                    None => format!("Rhein {} is out. This copy can't update itself; update it the way you installed it.", r.version),
                    Some(_) => format!("Rhein {} is out.", r.version),
                },
                urgent: false,
                action: Some(("What's new".into(), BannerAction::OpenUrl(r.page_url.clone()))),
            },
            Update::Installing(r) => Banner { key: format!("update:{}", r.version), text: format!("Installing Rhein {}…", r.version), urgent: false, action: None },
            Update::Installed(r) => Banner {
                key: format!("update-ready:{}", r.version),
                text: format!("Rhein {} is installed. Restart to use it.", r.version),
                urgent: false,
                action: Some(("Restart".into(), BannerAction::RestartForUpdate)),
            },
            _ => return None,
        })
    }
}

/// Where this copy may update itself, looked up once.
pub(super) fn target() -> Option<Target> {
    if !rhein_update::enabled() {
        return None;
    }
    let target = Target::detect();
    if target.is_none() {
        log::info!("update: {} can't be replaced from here; updates are only announced", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
    }
    target
}
