#![warn(clippy::all)]

use anyhow::Result;
use clap::Parser;
use md5::{Digest, Md5};
use serde_derive::Deserialize;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Parser)]
struct Args {
    #[arg(short, long)]
    config: String,
}

#[derive(Deserialize)]
struct Config {
    src_dir: String,
    dest_dir: String,

    include_list: Vec<String>,
    exclude_list: Option<Vec<String>>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("initializing");
    let args = Args::parse();

    log::info!("reading config \"{}\"", args.config);
    let config_str = match fs::read_to_string(&args.config) {
        Ok(config_str) => config_str,
        Err(err) => {
            log::error!("read \"{}\" to string error: {}", args.config, err);
            return;
        }
    };
    let config: Config = match toml::from_str(&config_str) {
        Ok(config) => config,
        Err(err) => {
            log::error!("parse config error: {}", err);
            return;
        }
    };

    log::info!("checking path \"{}\"", config.src_dir);
    let src_metadata = match fs::metadata(&config.src_dir) {
        Ok(src_metadata) => src_metadata,
        Err(err) => {
            log::error!("read metadata of \"{}\" error: {}", config.src_dir, err);
            return;
        }
    };
    if !src_metadata.is_dir() {
        log::error!(
            "check source path \"{}\" error: not a directory",
            config.src_dir
        );
        return;
    }

    log::info!("creating path \"{}\"", config.dest_dir);
    if let Err(err) = fs::create_dir_all(&config.dest_dir) {
        log::error!("create path \"{}\" error: {}", config.dest_dir, err);
        return;
    };

    log::info!("getting full path of src and dest");
    let src_dir = match fs::canonicalize(&config.src_dir) {
        Ok(src_dir) => src_dir,
        Err(err) => {
            log::error!("get absolute path of \"{}\" error: {}", config.src_dir, err);
            return;
        }
    };
    let dest_dir = match fs::canonicalize(&config.dest_dir) {
        Ok(dest_dir) => dest_dir,
        Err(err) => {
            log::error!(
                "get absolute path of \"{}\" error: {}",
                config.dest_dir,
                err
            );
            return;
        }
    };

    log::info!("traversing include files");
    let include_set = {
        let mut include_set = Vec::new();
        for entry in &config.include_list {
            if let Err(err) =
                traverse_files(&src_dir, entry, &mut include_set, &config.exclude_list)
            {
                log::error!(
                    "traverse files of \"{}/{}\" with exclude list error: {}",
                    src_dir.display(),
                    entry,
                    err
                );
                return;
            };
        }
        include_set
    };

    log::info!("traversing dest exist files");
    let dest_exist_set = {
        let mut dest_exist_set = Vec::new();
        if let Err(err) = traverse_files(&dest_dir, "", &mut dest_exist_set, &None) {
            log::error!(
                "traverse files of \"{}\" error: {}",
                dest_dir.display(),
                err
            );
            return;
        };
        dest_exist_set
    };

    log::info!("generating add/overwrite/remove list");
    let mut add_list = Vec::new();
    let mut overwrite_list = Vec::new();
    let mut remove_list = Vec::new();
    for entry in &include_set {
        let src_path = match path(&src_dir, entry) {
            Ok(src_path) => src_path,
            Err(err) => {
                log::error!(
                    "concat path of \"{}\" and \"{}\" error: {}",
                    src_dir.display(),
                    entry,
                    err
                );
                return;
            }
        };
        if src_path.is_dir() {
            continue;
        }

        if !dest_exist_set.contains(entry) {
            log::debug!("<- to: Add List -> {}", entry);
            add_list.push(entry.clone());
            continue;
        }

        let dest_path = match path(&dest_dir, entry) {
            Ok(dest_path) => dest_path,
            Err(err) => {
                log::error!(
                    "concat path of \"{}\" and \"{}\" error: {}",
                    dest_dir.display(),
                    entry,
                    err
                );
                return;
            }
        };
        log::debug!("[ Compare ] {} {}", src_path.display(), dest_path.display());
        if !match compare(&src_path, &dest_path) {
            Ok(result) => result,
            Err(err) => {
                log::error!(
                    "compare file \"{}\" and \"{}\" error: {}",
                    src_path.display(),
                    dest_path.display(),
                    err
                );
                return;
            }
        } {
            log::debug!("<- to: Overwrite List -> {}", entry);
            overwrite_list.push(entry.clone());
        }
    }
    for entry in &dest_exist_set {
        if !include_set.contains(entry) {
            log::debug!("<- to: Remove List -> {}", entry);
            remove_list.push(entry.clone());
        }
    }

    log::info!("removing");
    let remove_list_len = remove_list.len();
    for (i, entry) in remove_list.iter().enumerate() {
        if let Err(err) = remove(&dest_dir, entry) {
            log::warn!("remove \"{}/{}\" error: {}", dest_dir.display(), entry, err);
        };
        log::info!(
            "== {}% == | [REMOVE] {}",
            (i as f64 / remove_list_len as f64 * 100.0).trunc() as i64,
            entry
        );
    }

    log::info!("overwriting");
    let overwrite_list_len = overwrite_list.len();
    for (i, entry) in overwrite_list.iter().enumerate() {
        if let Err(err) = overwrite(&src_dir, &dest_dir, entry) {
            log::warn!(
                "overwrite \"{}/{}\" to \"{}/{}\" error: {}",
                src_dir.display(),
                entry,
                dest_dir.display(),
                entry,
                err
            );
        };
        log::info!(
            "== {}% == | [OVERWRITE] {}",
            (i as f64 / overwrite_list_len as f64 * 100.0).trunc() as i64,
            entry
        );
    }

    log::info!("adding");
    let add_list_len = add_list.len();
    for (i, entry) in add_list.iter().enumerate() {
        if let Err(err) = add(&src_dir, &dest_dir, entry) {
            log::warn!(
                "add \"{}/{}\" to \"{}/{}\" error: {}",
                src_dir.display(),
                entry,
                dest_dir.display(),
                entry,
                err
            );
        };
        log::info!(
            "== {}% == | [ADD] {}",
            (i as f64 / add_list_len as f64 * 100.0).trunc() as i64,
            entry
        );
    }
}

fn path(prefix: &Path, entry: &str) -> Result<PathBuf> {
    let mut path = prefix.to_path_buf();
    path.push(entry);
    Ok(path)
}

fn traverse_files(
    prefix: &Path,
    entry: &str,
    set: &mut Vec<String>,
    exclude: &Option<Vec<String>>,
) -> Result<()> {
    let path = path(prefix, entry)?;
    log::debug!("[ Traverse ] {}", path.display());
    if let Some(exclude) = exclude {
        for exclude_entry in exclude {
            if entry.starts_with(exclude_entry) {
                log::debug!("[! Skip !] {}", path.display());
                return Ok(());
            }
        }
    }

    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            traverse_files(
                prefix,
                &path.strip_prefix(prefix)?.display().to_string(),
                set,
                exclude,
            )?;
        }
    }

    if entry.is_empty() {
        return Ok(());
    }

    log::debug!("<- Insert -> {}", entry);
    set.push(entry.to_owned());

    Ok(())
}

fn compare(src_path: &Path, dest_path: &Path) -> Result<bool> {
    let mut src_file = File::open(src_path)?;
    let mut src_hasher = Md5::new();
    io::copy(&mut src_file, &mut src_hasher)?;
    let src_hash = src_hasher.finalize();

    let mut dest_file = File::open(dest_path)?;
    let mut dest_hasher = Md5::new();
    io::copy(&mut dest_file, &mut dest_hasher)?;
    let dest_hash = dest_hasher.finalize();

    Ok(src_hash == dest_hash)
}

fn remove(dest_dir: &Path, entry: &str) -> Result<()> {
    let path = path(dest_dir, entry)?;
    if path.is_dir() {
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }

    Ok(())
}

fn add(src_dir: &Path, dest_dir: &Path, entry: &str) -> Result<()> {
    let to_path = path(dest_dir, entry)?;
    if let Some(parent) = to_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(path(src_dir, entry)?, to_path)?;

    Ok(())
}

fn overwrite(src_dir: &Path, dest_dir: &Path, entry: &str) -> Result<()> {
    fs::copy(
        path(src_dir, entry)?,
        path(dest_dir, &format!("{entry}.reverso_src"))?,
    )?;
    fs::rename(
        path(dest_dir, entry)?,
        path(dest_dir, &format!("{entry}.reverso_dest"))?,
    )?;
    fs::rename(
        path(dest_dir, &format!("{entry}.reverso_src"))?,
        path(dest_dir, entry)?,
    )?;
    fs::remove_file(path(dest_dir, &format!("{entry}.reverso_dest"))?)?;

    Ok(())
}
