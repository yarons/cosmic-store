use cosmic::widget;
use libflatpak::{gio::Cancellable, prelude::*, Installation, Ref, RefKind, Transaction};
use std::{
    cell::Cell,
    collections::HashMap,
    error::Error,
    sync::{Arc, Mutex},
};

use super::{Backend, Package};
use crate::{AppInfo, AppstreamCache, OperationKind};

#[derive(Debug)]
pub struct Flatpak {
    appstream_cache: AppstreamCache,
}

impl Flatpak {
    pub fn new(locale: &str) -> Result<Self, Box<dyn Error>> {
        //TODO: should we support system installations?
        let inst = Installation::new_user(Cancellable::NONE)?;
        let mut paths = Vec::new();
        let mut icons_paths = Vec::new();
        for remote in inst.list_remotes(Cancellable::NONE)? {
            if let Some(appstream_dir) = remote.appstream_dir(None).and_then(|x| x.path()) {
                let xml_gz_path = appstream_dir.join("appstream.xml.gz");
                if xml_gz_path.is_file() {
                    paths.push(xml_gz_path);
                } else {
                    let xml_path = appstream_dir.join("appstream.xml");
                    if xml_path.is_file() {
                        paths.push(xml_path);
                    }
                }

                let icons_path = appstream_dir.join("icons");
                if icons_path.is_dir() {
                    icons_paths.push(icons_path);
                }
            }
        }

        // We don't store the installation because it is not Send
        Ok(Self {
            appstream_cache: AppstreamCache::new(paths, icons_paths, locale),
        })
    }

    fn ref_to_package<R: InstalledRefExt + RefExt>(r: R) -> Option<Package> {
        let id = r.name()?;

        let mut extra = HashMap::new();
        if let Some(arch) = r.arch() {
            extra.insert("arch".to_string(), arch.to_string());
        }
        if let Some(branch) = r.branch() {
            extra.insert("branch".to_string(), branch.to_string());
        }

        Some(Package {
            id: id.to_string(),
            //TODO: get icon from appstream data?
            icon: widget::icon::from_name(id.to_string()).size(128).handle(),
            name: r.appdata_name().unwrap_or(id).to_string(),
            summary: r.appdata_summary().map_or(String::new(), |x| x.to_string()),
            version: r.appdata_version().unwrap_or_default().to_string(),
            origin_opt: r.origin().map(|x| x.to_string()),
            extra,
        })
    }
}

impl Backend for Flatpak {
    fn load_cache(&mut self) -> Result<(), Box<dyn Error>> {
        self.appstream_cache.reload("flatpak");
        Ok(())
    }

    fn info_cache(&self) -> &AppstreamCache {
        &self.appstream_cache
    }

    fn installed(&self) -> Result<Vec<Package>, Box<dyn Error>> {
        //TODO: should we support system installations?
        let inst = Installation::new_user(Cancellable::NONE)?;
        let mut packages = Vec::new();
        //TODO: show non-desktop items?
        for r in inst.list_installed_refs_by_kind(RefKind::App, Cancellable::NONE)? {
            if let Some(package) = Self::ref_to_package(r) {
                packages.push(package);
            }
        }
        Ok(packages)
    }

    fn updates(&self) -> Result<Vec<Package>, Box<dyn Error>> {
        //TODO: should we support system installations?
        let inst = Installation::new_user(Cancellable::NONE)?;
        let mut packages = Vec::new();
        for r in inst.list_installed_refs_for_update(Cancellable::NONE)? {
            if let Some(package) = Self::ref_to_package(r) {
                packages.push(package);
            }
        }
        Ok(packages)
    }

    fn operation(
        &self,
        kind: OperationKind,
        id: &str,
        info: &AppInfo,
        callback: Box<dyn FnMut(f32) + 'static>,
    ) -> Result<(), Box<dyn Error>> {
        let callback = Arc::new(Mutex::new(callback));
        //TODO: should we support system installations?
        let inst = Installation::new_user(Cancellable::NONE)?;
        match kind {
            OperationKind::Install => {
                println!("{:#?}", info);
                for r_str in info.flatpak_refs.iter() {
                    let r = match Ref::parse(r_str) {
                        Ok(ok) => ok,
                        Err(err) => {
                            log::warn!("failed to parse flatpak ref {:?}: {}", r_str, err);
                            continue;
                        }
                    };
                    for remote in inst.list_remotes(Cancellable::NONE)? {
                        let Some(remote_name) = remote.name() else {
                            continue;
                        };
                        let remote_r = match inst.fetch_remote_ref_sync(
                            &remote_name,
                            r.kind(),
                            &r.name().unwrap_or_default(),
                            r.arch().as_deref(),
                            r.branch().as_deref(),
                            Cancellable::NONE,
                        ) {
                            Ok(ok) => ok,
                            Err(err) => {
                                log::info!("failed to find {} in {}: {}", id, remote_name, err);
                                continue;
                            }
                        };

                        log::info!("installing flatpak {} from remote {}", r_str, remote_name);
                        let total_ops = Arc::new(Cell::new(0));
                        let tx = Transaction::for_installation(&inst, Cancellable::NONE)?;
                        {
                            let total_ops = total_ops.clone();
                            tx.connect_ready(move |tx| {
                                total_ops.set(tx.operations().len());
                                true
                            });
                        }
                        let started_ops = Arc::new(Cell::new(0));
                        tx.connect_new_operation(move |_, op, progress| {
                            let current_op = started_ops.get();
                            started_ops.set(current_op + 1);
                            let progress_per_op =
                                100.0 / (total_ops.get().max(started_ops.get()) as f32);
                            log::info!(
                                "Operation {}: {} {:?}",
                                current_op,
                                op.operation_type(),
                                op.get_ref()
                            );
                            let callback = callback.clone();
                            progress.connect_changed(move |progress| {
                                log::info!(
                                    "{}: {}%",
                                    progress.status().unwrap_or_default(),
                                    progress.progress()
                                );
                                let op_progress = (progress.progress() as f32) / 100.0;
                                let total_progress =
                                    ((current_op as f32) + op_progress) * progress_per_op;
                                let mut callback = callback.lock().unwrap();
                                callback(total_progress)
                            });
                        });
                        tx.add_install(&remote_name, &r_str, &[])?;
                        tx.run(Cancellable::NONE)?;
                        return Ok(());
                    }
                }
                Err(format!("package {id} not found").into())
            }
        }
    }
}
