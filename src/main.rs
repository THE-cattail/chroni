use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{bail, Result};
use clap::Parser;
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
    let config = initialize()?;

    for task in config.tasks {
        if let Err(e) = process_task(&task, config.dry_run) {
            println!("{e}");
            if !food_rs::cli::ask_for_continue("with remain tasks")? {
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

    log::info!("Reading \"{}\"", args.config.display());
    let config_slice = food_rs::result!(
        fs::read(&args.config),
        "reading \"{}\" error: {}",
        args.config.display(),
    )?;
    let mut config: Config = food_rs::result!(
        food_rs::config::parse(&config_slice),
        "parsing config error: {}",
    )?;

    config.dry_run = args.dry_run;

    Ok(config)
}

fn process_task(task: &Task, dry_run: bool) -> Result<()> {
    let (src, dest) = get_src_dest_paths(task)?;

    log::info!("Collecting include files");
    let mut include_files = Vec::new();
    collect_files(
        &src,
        Path::new(""),
        &task.includes,
        task,
        &mut include_files,
    )?;

    if let Some(only_newest) = &task.only_newest {
        log::info!("Progressing only-newest lists");
        keep_only_newest(&src, &mut include_files, only_newest)?;
    }

    log::info!("Collecting exist files of destination directory");
    let mut dest_exist_files = Vec::new();
    collect_files(
        &dest,
        Path::new(""),
        &task.includes,
        task,
        &mut dest_exist_files,
    )?;

    log::info!("Generating to-do list");
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
    log::info!("Checking source path \"{}\"", task.src.display());
    if !task.src.is_dir() {
        anyhow::bail!(
            "check source path \"{}\" error: not a directory",
            task.src.display()
        );
    }

    log::info!("Creating destination path \"{}\"", task.dest.display());
    food_rs::result!(
        fs::create_dir_all(&task.dest),
        "create destination path \"{}\" error: {}",
        task.dest.display(),
    )?;

    log::info!("Getting absolute path of source and destination directory");
    Ok((
        food_rs::result!(
            fs::canonicalize(&task.src),
            "get absolute path of source path \"{}\" error: {}",
            task.src.display(),
        )?,
        food_rs::result!(
            fs::canonicalize(&task.dest),
            "get absolute path of destination path \"{}\" error: {}",
            task.dest.display(),
        )?,
    ))
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

lazy_static::lazy_static! {
    static ref ANY_PATTERN: Vec<String> = vec!["*".to_owned()];
}

fn collect_files(
    prefix: &Path,
    entry: &Path,
    includes: &[String],
    task: &Task,
    set: &mut Vec<PathBuf>,
) -> Result<()> {
    let path = prefix.join(entry);
    let path_str = path.display();

    log::trace!("collect_files({path_str})");

    if !task
        .requires
        .as_ref()
        .map_or(false, |requires| matches(entry, requires).is_some())
    {
        if let Some(excludes) = &task.excludes {
            if matches(entry, excludes).is_some() {
                log::trace!("matches(excludes: {path_str})");
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
        log::trace!("set.push({})", entry.display());
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
            let created = src.join(entry).metadata()?.created()?;

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

        let entry_str = entry.display();

        if !dest_exist_files.contains(entry) {
            log::debug!("  + To add: {entry_str}");
            add_list.push(entry.clone());
            continue;
        }

        let dest = dest.join(entry);

        let src_str = src.display();
        let dest_str = dest.display();

        log::debug!("? Comparing: \"{src_str}\" & \"{dest_str}\"");
        if need_overwrite(&src, &dest, overwrite_mode)? {
            log::debug!("  ~ Skiped: {entry_str}");
        } else {
            log::debug!("  ^ To overwrite: {entry_str}");
            overwrite_list.push(entry.clone());
        }
    }

    for entry in dest_exist_files {
        if !include_files.contains(entry) {
            log::debug!("  - To remove: {}", entry.display());
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

    let src_str = src.display();
    let dest_str = dest.display();

    if food_rs::result!(
        src.metadata(),
        "get metadata of source file \"{src_str}\" error: {}",
    )?
    .len()
        != food_rs::result!(
            dest.metadata(),
            "get metadata of destination file \"{dest_str}\" error: {}",
        )?
        .len()
    {
        return Ok(false);
    }

    if mode == &OverwriteMode::FastCompare {
        return Ok(true);
    }

    let mut src_file = food_rs::result!(
        File::open(src),
        "open source file \"{}\" error: {}",
        src_str,
    )?;
    let mut src_hasher = Sha1::new();
    food_rs::result!(
        io::copy(&mut src_file, &mut src_hasher),
        "copy source file \"{src_str}\" to hasher error: {}",
    )?;
    let src_hash = src_hasher.finalize();

    let mut dest_file = food_rs::result!(
        File::open(dest),
        "open destination file \"{dest_str}\" error: {}",
    )?;
    let mut dest_hasher = Sha1::new();
    food_rs::result!(
        io::copy(&mut dest_file, &mut dest_hasher),
        "copy destination file \"{dest_str}\" to hasher error: {}",
    )?;
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
        log::info!("Execute {name} list");
        for (i, entry) in list.iter().enumerate() {
            if let Err(e) = (f)(src, dest, entry) {
                log::warn!("{e}");
            };
            log::info!(
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
    let path_str = path.display();

    log::debug!("* - Removing: {path_str}");

    food_rs::result!(
        if path.is_dir() {
            log::trace!("remove dir");
            fs::remove_dir(&path)
        } else {
            log::trace!("remove file");
            fs::remove_file(&path)
        },
        "remove \"{path_str}\" error: {}",
    )?;

    Ok(())
}

const SUFFIX_CHRONI_SRC: &str = "chroni_src";
const SUFFIX_CHRONI_DEST: &str = "chroni_dest";

fn overwrite(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let src = src.join(entry);
    let src_str = src.display();

    let dest = dest.join(entry);
    let dest_str = dest.display();

    let dest_new_tmp = {
        let mut path = dest.clone();
        path.set_extension(SUFFIX_CHRONI_SRC);
        path
    };
    let dest_new_tmp_str = dest_new_tmp.display();

    let dest_old_tmp = {
        let mut path = dest.clone();
        path.set_extension(SUFFIX_CHRONI_DEST);
        path
    };
    let dest_old_tmp_str = dest_old_tmp.display();

    log::debug!("* ^ Overwriting: {src_str} -> {dest_str}");

    log::trace!("copy({src_str}, {dest_new_tmp_str})");
    food_rs::result!(
        fs::copy(&src, &dest_new_tmp),
        "copy \"{src_str}\" for overwriting \"{dest_str}\" error: {}",
    )?;

    log::trace!("rename({dest_str}, {dest_old_tmp_str})");
    food_rs::result!(
        fs::rename(&dest, &dest_old_tmp),
        "rename \"{dest_str}\" for overwriting \"{dest_str}\" error: {}",
    )?;

    log::trace!("rename({dest_new_tmp_str}, {dest_str})");
    food_rs::result!(
        fs::rename(&dest_new_tmp, &dest),
        "rename \"{dest_new_tmp_str}\" for overwriting \"{dest_str}\" error: {}",
    )?;

    log::trace!("remove({dest_old_tmp_str})");
    food_rs::result!(
        fs::remove_file(&dest_old_tmp),
        "remove \"{dest_old_tmp_str}\" for overwriting \"{dest_str}\" error: {}",
    )?;

    Ok(())
}

fn add(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let src = src.join(entry);
    let dest = dest.join(entry);

    let src_str = src.display();
    let dest_str = dest.display();

    log::debug!("* + Adding: {src_str} -> {dest_str}");

    if let Some(parent) = dest.parent() {
        log::trace!("create dir all");
        food_rs::result!(
            fs::create_dir_all(parent),
            "create directory \"{}\" for adding \"{}\" error: {}",
            parent.display(),
            entry.display(),
        )?;
    }

    food_rs::result!(fs::copy(&src, dest), "copy \"{src_str}\" error: {}",)?;

    Ok(())
}
