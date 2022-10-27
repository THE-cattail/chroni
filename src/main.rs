use anyhow::Result;
use clap::Parser;
use md5::{Digest, Md5};
use serde_derive::Deserialize;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Task {
    src: PathBuf,
    dest: PathBuf,

    includes: Vec<PathBuf>,
    excludes: Option<Vec<PathBuf>>,
}

#[derive(Deserialize)]
struct Config {
    tasks: Vec<Task>,
}

fn main() -> Result<()> {
    let config = initialize()?;

    for task in config.tasks {
        if let Err(e) = process_task(&task) {
            println!(
                "process backup task for \"{}\" error: {}",
                task.src.display(),
                e
            );
            print!("Continue? (Y/n): ");
            assert!(!food_rs::cli::ask_for_continue()?, "user aborted.");
        }
    }

    Ok(())
}

#[derive(Parser)]
#[command(author, version, about)]
struct CliArgs {
    #[arg(short, long, value_name = "FILE", help = "Specify configuration file", value_hint = clap::ValueHint::FilePath, default_value = "config")]
    config: PathBuf,
    #[arg(long = "dry-run", default_value = "false")]
    dry_run: bool,
}

fn initialize() -> Result<Config> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = CliArgs::parse();

    log::info!("Reading \"{}\"", args.config.display());
    let config_slice = food_rs::result!(
        fs::read(&args.config),
        "reading \"{}\" failed: {}",
        args.config.display(),
    )?;
    food_rs::result!(
        food_rs::config::parse(&config_slice),
        "parsing config failed: {}",
    )
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

fn process_task(task: &Task) -> Result<()> {
    let (src, dest) = get_src_dest_paths(task)?;

    log::info!("Collecting include files");
    let include_files = collect_include_files(&src, &task.includes, &task.excludes)?;

    log::info!("Collecting exist files of destination directory");
    let dest_exist_files = collect_dest_exist_files(&dest)?;

    log::info!("Generating to-do list");
    let (add_list, overwrite_list, remove_list) =
        generate_to_do_list(&src, &dest, &include_files, &dest_exist_files)?;

    if !remove_list.is_empty() {
        let remove_list_len = remove_list.len();
        log::info!("Execute remove list");
        for (i, entry) in remove_list.iter().enumerate() {
            if let Err(e) = remove(&dest, entry) {
                log::warn!("{e}");
            };
            log::info!(
                "[ {}% ] | [REMOVE] {}",
                i * 100 / remove_list_len,
                entry.display()
            );
        }
    }

    if !overwrite_list.is_empty() {
        log::info!("Execute overwrite list");
        let overwrite_list_len = overwrite_list.len();
        for (i, entry) in overwrite_list.iter().enumerate() {
            if let Err(e) = overwrite(&src, &dest, entry) {
                log::warn!("{e}");
            };
            log::info!(
                "[ {}% ] | [OVERWRITE] {}",
                i * 100 / overwrite_list_len,
                entry.display()
            );
        }
    }

    if !add_list.is_empty() {
        log::info!("Execute add list");
        let add_list_len = add_list.len();
        for (i, entry) in add_list.iter().enumerate() {
            if let Err(e) = add(&src, &dest, entry) {
                log::warn!("{e}");
            };
            log::info!(
                "[ {}% ] | [ADD] {}",
                i * 100 / add_list_len,
                entry.display()
            );
        }
    }

    println!("chroni: Backup complete.");

    Ok(())
}

fn collect_files(
    prefix: &Path,
    entry: &Path,
    set: &mut Vec<PathBuf>,
    exclude: &Option<Vec<PathBuf>>,
) -> Result<()> {
    let path = prefix.join(entry);
    let path_str = path.display();
    log::debug!("Traversing: \"{}\"", path_str);

    if let Some(exclude) = exclude {
        for exclude_entry in exclude {
            if entry.starts_with(exclude_entry) {
                log::debug!("  ~ Skiped: \"{}\"", path_str);
                return Ok(());
            }
        }
    }

    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_files(prefix, entry?.path().strip_prefix(prefix)?, set, exclude)?;
        }
    }

    if entry == Path::new("") {
        return Ok(());
    }

    log::debug!("  > Collecting: \"{}\"", entry.display());
    set.push(entry.to_owned());

    Ok(())
}

fn collect_include_files(
    src: &Path,
    includes: &Vec<PathBuf>,
    excludes: &Option<Vec<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    let mut include_set = Vec::new();
    for entry in includes {
        food_rs::result!(
            collect_files(src, entry, &mut include_set, excludes),
            "traverse include files of \"{}\" with excludes error: {}",
            src.join(entry).display(),
        )?;
    }
    Ok(include_set)
}

fn collect_dest_exist_files(dest: &Path) -> Result<Vec<PathBuf>> {
    let mut dest_exist_set = Vec::new();
    food_rs::result!(
        collect_files(dest, Path::new(""), &mut dest_exist_set, &None),
        "traverse exist files of destination directory \"{}\" error: {}",
        dest.display(),
    )?;
    Ok(dest_exist_set)
}

fn generate_to_do_list(
    src: &Path,
    dest: &Path,
    include_files: &[PathBuf],
    dest_exist_files: &[PathBuf],
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
            log::debug!("  + To add: {}", entry_str);
            add_list.push(entry.clone());
            continue;
        }

        let dest = dest.join(entry);

        let src_str = src.display();
        let dest_str = dest.display();

        log::debug!("? Comparing: \"{}\" & \"{}\"", src_str, dest_str);
        if food_rs::result!(
            file_same(&src, &dest),
            "compare file \"{}\" with \"{}\" error: {}",
            src_str,
            dest_str,
        )? {
            log::debug!("  ~ Skiped: {}", entry_str);
        } else {
            log::debug!("  ^ To overwrite: {}", entry_str);
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

fn file_same(src: &Path, dest: &Path) -> Result<bool> {
    let src_str = src.display();
    let dest_str = dest.display();

    let src_metadata = food_rs::result!(
        src.metadata(),
        "get metadata of source file \"{}\" error: {}",
        src_str,
    )?;
    let dest_metadata = food_rs::result!(
        dest.metadata(),
        "get metadata of destination file \"{}\" error: {}",
        dest_str,
    )?;

    if src_metadata.len() != dest_metadata.len() {
        return Ok(false);
    }
    if food_rs::result!(
        src_metadata.modified(),
        "get modified time of source file \"{}\" error: {}",
        src_str,
    )? != food_rs::result!(
        dest_metadata.modified(),
        "get modified time of destination file \"{}\" error: {}",
        dest_str,
    )? {
        return Ok(false);
    }

    let mut src_file = food_rs::result!(
        File::open(src),
        "open source file \"{}\" error: {}",
        src_str,
    )?;
    let mut src_hasher = Md5::new();
    food_rs::result!(
        io::copy(&mut src_file, &mut src_hasher),
        "copy source file \"{}\" to hasher error: {}",
        src_str,
    )?;
    let src_hash = src_hasher.finalize();

    let mut dest_file = food_rs::result!(
        File::open(dest),
        "open destination file \"{}\" error: {}",
        dest_str,
    )?;
    let mut dest_hasher = Md5::new();
    food_rs::result!(
        io::copy(&mut dest_file, &mut dest_hasher),
        "copy destination file \"{}\" to hasher error: {}",
        dest_str,
    )?;
    let dest_hash = dest_hasher.finalize();

    Ok(src_hash == dest_hash)
}

fn remove(dest: &Path, entry: &Path) -> Result<()> {
    let path = dest.join(entry);

    food_rs::result!(
        if path.is_dir() {
            fs::remove_dir(&path)
        } else {
            fs::remove_file(&path)
        },
        "remove \"{}\" error: {}",
        path.display(),
    )?;

    Ok(())
}

fn add(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let dest = dest.join(entry);

    if let Some(parent) = dest.parent() {
        food_rs::result!(
            fs::create_dir_all(parent),
            "create directory \"{}\" for adding \"{}\" error: {}",
            parent.display(),
            entry.display(),
        )?;
    }

    let src = src.join(entry);
    food_rs::result!(fs::copy(&src, dest), "copy \"{}\" error: {}", src.display(),)?;

    Ok(())
}

const SUFFIX_CHRONI_SRC: &str = ".chroni_src";
const SUFFIX_CHRONI_DEST: &str = ".chroni_dest";

fn overwrite(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let src = src.join(entry);

    let dest = dest.join(entry);
    let dest_str = dest.display();

    let dest_new_tmp = {
        let mut entry = entry.to_owned();
        entry.push(SUFFIX_CHRONI_SRC);
        entry
    };
    let dest_new_tmp_str = dest_new_tmp.display();

    let dest_old_tmp = {
        let mut entry = entry.to_owned();
        entry.push(SUFFIX_CHRONI_DEST);
        entry
    };
    let dest_old_tmp_str = dest_old_tmp.display();

    food_rs::result!(
        fs::copy(&src, &dest_new_tmp),
        "copy \"{}\" for overwriting \"{}\" error: {}",
        src.display(),
        dest_str,
    )?;
    food_rs::result!(
        fs::rename(&dest, &dest_old_tmp),
        "rename \"{}\" for overwriting \"{}\" error: {}",
        dest_str,
        dest_str,
    )?;
    food_rs::result!(
        fs::rename(&dest_new_tmp, &dest),
        "rename \"{}\" for overwriting \"{}\" error: {}",
        dest_new_tmp_str,
        dest_str,
    )?;
    food_rs::result!(
        fs::remove_file(&dest_old_tmp),
        "remove \"{}\" for overwriting \"{}\" error: {}",
        dest_old_tmp_str,
        dest_str,
    )?;

    Ok(())
}
