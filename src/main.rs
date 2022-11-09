use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use clap::Parser;
use color_eyre::eyre;
use eyre::{bail, Result, WrapErr};
use inquire::Confirm;
use once_cell::sync::Lazy;
use serde_derive::Deserialize;
use sha1::{Digest, Sha1};
use wildmatch::WildMatch;

#[derive(PartialEq)]
#[derive(Deserialize)]
#[derive(Default)]
enum OverwriteMode {
    #[serde(rename = "any")]
    Any,

    #[serde(rename = "deep_compare")]
    DeepCompare,

    #[default]
    #[serde(rename = "fast_compare")]
    FastCompare,

    #[serde(rename = "never")]
    Never,
}

#[derive(Deserialize)]
struct Task {
    src:  PathBuf,
    dest: PathBuf,

    #[serde(default = "OverwriteMode::default")]
    overwrite_mode: OverwriteMode,

    includes: Vec<String>,
    excludes: Option<Vec<String>>,
    requires: Option<Vec<String>>,

    only_newest: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Config {
    tasks: Vec<Task>,

    #[serde(skip_deserializing)]
    dry_run: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let config = initialize()?;

    for task in config.tasks {
        if let Err(e) = process_task(&task, config.dry_run) {
            println!("{e}");
            if !Confirm::new("Continue with the remaining tasks?")
                .with_default(true)
                .prompt()?
            {
                bail!("user aborted.");
            };
        }
    }

    println!("chroni: All tasks done.");
    Ok(())
}

#[derive(Parser)]
#[command(author, version, about)]
struct CliArgs {
    #[arg(short, long, value_name = "FILE", help = "Specify configuration file",
    value_hint = clap::ValueHint::FilePath, default_value = "config")]
    config:  PathBuf,
    #[arg(
        long = "dry-run",
        help = "Run without actual file operations",
        default_value = "false"
    )]
    dry_run: bool,
}

fn initialize() -> Result<Config> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = CliArgs::parse();
    let config_disp = args.config.display();

    tracing::info!("Reading \"{}\"", config_disp);
    let config_slice = fs::read(&args.config)
        .wrap_err_with(|| format!("Failed to read config file: {config_disp}"))?;
    let mut config: Config = toml::from_slice(&config_slice).wrap_err("Failed to parse config")?;

    config.dry_run = args.dry_run;

    Ok(config)
}

fn process_task(task: &Task, dry_run: bool) -> Result<()> {
    let (src, dest) = get_src_dest_paths(task)?;

    tracing::info!("Collecting include files");
    let mut include_files = Vec::new();
    collect_files(
        &src,
        Path::new(""),
        &task.includes,
        task,
        &mut include_files,
    )?;

    if let Some(only_newest) = &task.only_newest {
        tracing::info!("Progressing only-newest lists");
        keep_only_newest(&src, &mut include_files, only_newest)?;
    }

    tracing::info!("Collecting exist files of destination directory");
    let mut dest_exist_files = Vec::new();
    collect_files(
        &dest,
        Path::new(""),
        &task.includes,
        task,
        &mut dest_exist_files,
    )?;

    tracing::info!("Generating to-do list");
    let (add_list, overwrite_list, remove_list) = generate_to_do_list(
        &src,
        &dest,
        &include_files,
        &dest_exist_files,
        &task.overwrite_mode,
    )?;

    if dry_run {
        println!("chroni: Dry-run task for {} done.", src.display());

        return Ok(());
    }

    execute_list("remove", &remove_list, remove, &src, &dest);
    execute_list("overwrite", &overwrite_list, overwrite, &src, &dest);
    execute_list("add", &add_list, add, &src, &dest);

    println!("chroni: Task for {} done.", src.display());

    Ok(())
}

fn get_src_dest_paths(task: &Task) -> Result<(PathBuf, PathBuf)> {
    tracing::info!("Checking source path \"{}\"", task.src.display());
    if !task.src.is_dir() {
        eyre::bail!(
            "check source path \"{}\" error: not a directory",
            task.src.display()
        );
    }

    tracing::info!("Creating destination path \"{}\"", task.dest.display());
    fs::create_dir_all(&task.dest)
        .wrap_err_with(|| format!("Failed to create destination path: {}", task.dest.display(),))?;

    tracing::info!("Getting absolute path of source and destination directory");
    let src = fs::canonicalize(&task.src).wrap_err_with(|| {
        format!(
            "Failed to get absolute path of source directory: {}",
            task.src.display(),
        )
    })?;
    let dest = fs::canonicalize(&task.dest).wrap_err_with(|| {
        format!(
            "Failed to get absolute path of destination directory: {}",
            task.dest.display(),
        )
    })?;
    Ok((src, dest))
}

fn matches(entry: &Path, patterns: &[String]) -> Option<String> {
    for p in patterns {
        if (p == "." && entry == Path::new(""))
            || WildMatch::new(p).matches(&entry.display().to_string())
        {
            return Some(p.clone());
        };
    }

    None
}

static ANY_PATTERN: Lazy<Vec<String>> = Lazy::new(|| vec!["*".to_owned()]);

fn collect_files(
    prefix: &Path,
    entry: &Path,
    includes: &[String],
    task: &Task,
    set: &mut Vec<PathBuf>,
) -> Result<()> {
    let path = prefix.join(entry);
    let path_disp = path.display();

    tracing::trace!("collect_files({path_disp})");

    if !task
        .requires
        .as_ref()
        .map_or(false, |requires| matches(entry, requires).is_some())
    {
        if let Some(excludes) = &task.excludes {
            if matches(entry, excludes).is_some() {
                tracing::trace!("matches(excludes: {path_disp})");
                return Ok(());
            }
        }
    }

    let include = matches(entry, includes).is_some();

    if path.is_dir() {
        for entry in fs::read_dir(&path)? {
            collect_files(
                prefix,
                entry?.path().strip_prefix(prefix)?,
                if include { &ANY_PATTERN } else { includes },
                task,
                set,
            )?;
        }
    }

    if include {
        tracing::trace!("set.push({})", entry.display());
        set.push(entry.to_owned());
    }

    Ok(())
}
struct Newest {
    entry:   PathBuf,
    created: SystemTime,
}

fn keep_only_newest(
    src: &Path,
    include_files: &mut Vec<PathBuf>,
    only_newest: &[String],
) -> Result<()> {
    let mut m: HashMap<String, Newest> = HashMap::new();
    let mut to_removes = HashSet::new();

    for entry in &*include_files {
        if let Some(pattern) = matches(entry, only_newest) {
            let created = src
                .join(entry)
                .metadata()?
                .created()
                .map_or_else(|_| SystemTime::now(), |created| created);

            let insert = m.get(&pattern).map_or(true, |newest| {
                if created > newest.created {
                    to_removes.insert(newest.entry.clone());
                    true
                } else {
                    to_removes.insert(entry.clone());
                    false
                }
            });

            if insert {
                m.insert(
                    pattern,
                    Newest {
                        entry: entry.clone(),
                        created,
                    },
                );
            }
        }
    }

    include_files.retain(|e| !to_removes.contains(e));

    Ok(())
}

fn generate_to_do_list(
    src: &Path,
    dest: &Path,
    include_files: &[PathBuf],
    dest_exist_files: &[PathBuf],
    overwrite_mode: &OverwriteMode,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>)> {
    let mut add_list = Vec::new();
    let mut overwrite_list = Vec::new();
    let mut remove_list = Vec::new();

    for entry in include_files {
        let src = src.join(entry);
        if src.is_dir() {
            continue;
        }

        let entry_disp = entry.display();

        if !dest_exist_files.contains(entry) {
            tracing::debug!("  + To add: {entry_disp}");
            add_list.push(entry.clone());
            continue;
        }

        let dest = dest.join(entry);

        let src_disp = src.display();
        let dest_disp = dest.display();

        tracing::debug!("? Comparing: \"{src_disp}\" & \"{dest_disp}\"");
        if need_overwrite(&src, &dest, overwrite_mode)? {
            tracing::debug!("  ~ Skiped: {entry_disp}");
        } else {
            tracing::debug!("  ^ To overwrite: {entry_disp}");
            overwrite_list.push(entry.clone());
        }
    }

    for entry in dest_exist_files {
        if !include_files.contains(entry) {
            tracing::debug!("  - To remove: {}", entry.display());
            remove_list.push(entry.clone());
        }
    }

    Ok((add_list, overwrite_list, remove_list))
}

fn need_overwrite(src: &Path, dest: &Path, mode: &OverwriteMode) -> Result<bool> {
    if mode == &OverwriteMode::Any {
        return Ok(false);
    }
    if mode == &OverwriteMode::Never {
        return Ok(true);
    }

    let src_disp = src.display();
    let dest_disp = dest.display();

    let src_len = src
        .metadata()
        .wrap_err_with(|| format!("Failed to get metadata of source file: {src_disp}",))?
        .len();
    let dest_len = dest
        .metadata()
        .wrap_err_with(|| format!("Failed to get metadata of destination file: {dest_disp}",))?
        .len();
    if src_len != dest_len {
        return Ok(false);
    }

    if mode == &OverwriteMode::FastCompare {
        return Ok(true);
    }

    let mut src_file =
        File::open(src).wrap_err_with(|| format!("Failed to open source file: {src_disp}"))?;
    let mut src_hasher = Sha1::new();
    io::copy(&mut src_file, &mut src_hasher)
        .wrap_err_with(|| format!("Failed to copy source file to hasher: {src_disp}",))?;
    let src_hash = src_hasher.finalize();

    let mut dest_file = File::open(dest)
        .wrap_err_with(|| format!("Failed to open destination file: {dest_disp}",))?;
    let mut dest_hasher = Sha1::new();
    io::copy(&mut dest_file, &mut dest_hasher)
        .wrap_err_with(|| format!("Failed to copy destination file to hasher: {dest_disp}",))?;
    let dest_hash = dest_hasher.finalize();

    Ok(src_hash == dest_hash)
}

fn execute_list(
    name: &str,
    list: &[PathBuf],
    f: fn(&Path, &Path, &Path) -> Result<()>,
    src: &Path,
    dest: &Path,
) {
    if !list.is_empty() {
        let list_len = list.len();
        tracing::info!("Execute {name} list");
        for (i, entry) in list.iter().enumerate() {
            if let Err(e) = (f)(src, dest, entry) {
                tracing::warn!("{e}");
            };
            tracing::info!(
                "[ {}% ] | [{}] {}",
                (i + 1) * 100 / list_len,
                name.to_uppercase(),
                entry.display()
            );
        }
    }
}

fn remove(_: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let path = dest.join(entry);
    let path_disp = path.display();

    tracing::debug!("* - Removing: {path_disp}");

    if path.is_dir() {
        tracing::trace!("remove dir");
        fs::remove_dir(&path)
    } else {
        tracing::trace!("remove file");
        fs::remove_file(&path)
    }
    .wrap_err_with(|| format!("Failed to remove: {path_disp}"))?;

    Ok(())
}

const SUFFIX_CHRONI_SRC: &str = "chroni_src";
const SUFFIX_CHRONI_DEST: &str = "chroni_dest";

fn overwrite(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let src = src.join(entry);
    let src_disp = src.display();

    let dest = dest.join(entry);
    let dest_disp = dest.display();

    let dest_new_tmp = {
        let mut path = dest.clone();
        path.set_extension(SUFFIX_CHRONI_SRC);
        path
    };
    let dest_new_tmp_disp = dest_new_tmp.display();

    let dest_old_tmp = {
        let mut path = dest.clone();
        path.set_extension(SUFFIX_CHRONI_DEST);
        path
    };
    let dest_old_tmp_disp = dest_old_tmp.display();

    tracing::debug!("* ^ Overwriting: {src_disp} -> {dest_disp}");

    tracing::trace!("copy({src_disp}, {dest_new_tmp_disp})");
    fs::copy(&src, &dest_new_tmp)
        .wrap_err_with(|| format!("Failed to copy for overwriting {dest_disp}: {src_disp}"))?;

    tracing::trace!("rename({dest_disp}, {dest_old_tmp_disp})");
    fs::rename(&dest, &dest_old_tmp)
        .wrap_err_with(|| format!("Failed to rename for overwriting {dest_disp}: {dest_disp}"))?;

    tracing::trace!("rename({dest_new_tmp_disp}, {dest_disp})");
    fs::rename(&dest_new_tmp, &dest)
        .wrap_err_with(|| format!("rename for overwriting {dest_disp}: {dest_new_tmp_disp}"))?;

    tracing::trace!("remove({dest_old_tmp_disp})");
    fs::remove_file(&dest_old_tmp)
        .wrap_err_with(|| format!("remove for overwriting {dest_disp}: {dest_old_tmp_disp}"))?;

    Ok(())
}

fn add(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let src = src.join(entry);
    let dest = dest.join(entry);

    let src_disp = src.display();
    let dest_disp = dest.display();

    tracing::debug!("* + Adding: {src_disp} -> {dest_disp}");

    if let Some(parent) = dest.parent() {
        tracing::trace!("create dir all");
        fs::create_dir_all(parent).wrap_err_with(|| {
            format!(
                "Failed to create directory for adding {} error: {}",
                entry.display(),
                parent.display(),
            )
        })?;
    }

    fs::copy(&src, dest).wrap_err_with(|| format!("Failed to copy: {src_disp}"))?;

    Ok(())
}
