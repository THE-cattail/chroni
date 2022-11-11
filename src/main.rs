use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum, ValueHint};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use indicatif::ProgressBar;
use sha1::{Digest, Sha1};

#[derive(Clone, ValueEnum, PartialEq)]
enum OverwriteMode {
    #[value(help = "always overwrite")]
    Any,
    #[value(help = "overwrite when sizes of the source and the destination are different")]
    FastComp,
    #[value(help = "overwrite when hashes(SHA-1) of the source and the destination are different")]
    DeepComp,
    #[value(help = "never overwrite")]
    None,
}

#[derive(Parser)]
#[command(author, version, about)]
struct Task {
    #[arg(value_name = "SRC_DIR", help = "The source directory of the backup task", value_hint = ValueHint::DirPath)]
    src:            PathBuf,
    #[arg(value_name = "DEST_DIR", help = "The destination directory of the backup task", value_hint = ValueHint::DirPath)]
    dest:           PathBuf,
    #[arg(
        value_enum,
        short,
        long = "overwrite-mode",
        value_name = "MODE",
        help = "Specify the mode for checking if a destination file should be overwritten",
        default_value_t = OverwriteMode::FastComp,
    )]
    overwrite_mode: OverwriteMode,
    #[arg(
        long = "only_newest",
        value_name = "GLOB",
        help = "Set the filter of directories which only keep the newest file in it, can be used \
                multiple times"
    )]
    only_newest:    Vec<Glob>,
    #[arg(
        long = "dry-run",
        help = "Run without actual file operations",
        default_value_t = false
    )]
    dry_run:        bool,
}

fn main() -> Result<()> {
    let task = initialize();
    process_task(&task)?;
    Ok(())
}

fn initialize() -> Task {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    Task::parse()
}

fn process_task(task: &Task) -> Result<()> {
    let (src, dest) = get_src_dest_paths(task)?;

    log::info!("Collecting include files");
    let mut include_files = Vec::new();
    collect_files(&src, &mut include_files).context("Failed to collect include files")?;

    if !task.only_newest.is_empty() {
        log::info!("Excluding old-version files insides only-newest directories");
        keep_only_newest(
            &src,
            &mut include_files,
            &generate_globset(&task.only_newest)
                .context("Failed to generate globset from only-newest list")?,
        )?;
    }

    log::info!("Collecting exist files of destination directory");
    let mut dest_exist_files = Vec::new();
    collect_files(&dest, &mut dest_exist_files)
        .context("Failed to collect exist files of destination directory")?;

    log::info!("Generating to-do list");
    let (add_list, overwrite_list, remove_list) = generate_to_do_list(
        &src,
        &dest,
        &include_files,
        &dest_exist_files,
        &task.overwrite_mode,
    )
    .context("Failed to generate to-do list")?;

    if !task.dry_run {
        execute_list("remove", &remove_list, remove, &src, &dest)
            .context("Failed to execute remove list")?;
        execute_list("overwrite", &overwrite_list, copy, &src, &dest)
            .context("Failed to execute overwrite list")?;
        execute_list("add", &add_list, copy, &src, &dest).context("Failed to execute add list")?;
    }

    println!("chroni: Task done.");

    Ok(())
}

fn generate_globset(gs: &[Glob]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for g in gs {
        builder.add(g.clone());

        let g_dir_str = format!("{}/*", g.glob());
        let g_dir = Glob::new(&g_dir_str)
            .with_context(|| format!("Failed to generate glob: {g_dir_str}"))?;
        builder.add(g_dir);
    }
    Ok(builder.build()?)
}

fn get_src_dest_paths(task: &Task) -> Result<(PathBuf, PathBuf)> {
    log::info!("Checking source path \"{}\"", task.src.display());
    if !task.src.is_dir() {
        anyhow::bail!("Source path is not a directory");
    }

    log::info!("Creating destination path \"{}\"", task.dest.display());
    fs::create_dir_all(&task.dest).context("Failed to create destination path")?;

    log::info!("Getting absolute path of source and destination directory");
    let src =
        fs::canonicalize(&task.src).context("Failed to get absolute path of source directory")?;
    let dest = fs::canonicalize(&task.dest)
        .context("Failed to get absolute path of destination directory")?;
    Ok((src, dest))
}

fn collect_files(prefix: &Path, set: &mut Vec<PathBuf>) -> Result<()> {
    for entry in WalkBuilder::new(prefix).hidden(false).build() {
        let entry = entry.context("Failed to get DirEntry when walking")?;
        let path = entry
            .path()
            .strip_prefix(prefix)
            .context("Failed to strip prefix")?;
        set.push(path.to_owned());
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
    only_newest: &GlobSet,
) -> Result<()> {
    let mut m: HashMap<usize, Newest> = HashMap::new();
    let mut to_removes = HashSet::new();

    for entry in &*include_files {
        for seq in only_newest.matches(entry) {
            let path = src.join(entry);
            let metadata = path
                .metadata()
                .with_context(|| format!("Failed to get metadata: {}", path.display()))?;
            let created = metadata
                .created()
                .map_or_else(|_| SystemTime::now(), |created| created);

            let insert = m.get(&seq).map_or(true, |newest| {
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
                    seq,
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
            log::debug!("  + To add: {entry_disp}");
            add_list.push(entry.clone());
            continue;
        }

        let dest = dest.join(entry);

        let src_disp = src.display();
        let dest_disp = dest.display();

        log::debug!("? Comparing: \"{src_disp}\" & \"{dest_disp}\"");
        if need_overwrite(&src, &dest, overwrite_mode).with_context(|| {
            format!(
                "Failed to check overwrite: {} -> {}",
                src.display(),
                dest.display()
            )
        })? {
            log::debug!("  ~ Skiped: {entry_disp}");
        } else {
            log::debug!("  ^ To overwrite: {entry_disp}");
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
    if mode == &OverwriteMode::None {
        return Ok(true);
    }

    let src_disp = src.display();
    let dest_disp = dest.display();

    let src_len = src
        .metadata()
        .with_context(|| format!("Failed to get metadata of source file: {src_disp}",))?
        .len();
    let dest_len = dest
        .metadata()
        .with_context(|| format!("Failed to get metadata of destination file: {dest_disp}",))?
        .len();
    if src_len != dest_len {
        return Ok(false);
    }

    if mode == &OverwriteMode::FastComp {
        return Ok(true);
    }

    let mut src_file =
        File::open(src).with_context(|| format!("Failed to open source file: {src_disp}"))?;
    let mut src_hasher = Sha1::new();
    io::copy(&mut src_file, &mut src_hasher)
        .with_context(|| format!("Failed to copy source file to hasher: {src_disp}"))?;
    let src_hash = src_hasher.finalize();

    let mut dest_file = File::open(dest)
        .with_context(|| format!("Failed to open destination file: {dest_disp}"))?;
    let mut dest_hasher = Sha1::new();
    io::copy(&mut dest_file, &mut dest_hasher)
        .with_context(|| format!("Failed to copy destination file to hasher: {dest_disp}"))?;
    let dest_hash = dest_hasher.finalize();

    Ok(src_hash == dest_hash)
}

fn execute_list(
    name: &str,
    list: &[PathBuf],
    f: fn(&Path, &Path, &Path) -> Result<()>,
    src: &Path,
    dest: &Path,
) -> Result<()> {
    if !list.is_empty() {
        let list_len = list.len();
        log::info!("Execute {name} list");
        let bar = ProgressBar::new(
            list_len
                .try_into()
                .context("Failed to get ProgressBar length")?,
        );
        for (i, entry) in list.iter().enumerate() {
            if let Err(e) = (f)(src, dest, entry) {
                log::warn!("Failed to execute {name} task:\n{e}");
            };
            bar.inc(1);
            log::info!(
                "[ {}% ] | [{}] {}",
                (i + 1) * 100 / list_len,
                name.to_uppercase(),
                entry.display()
            );
        }
        bar.finish();
    }

    Ok(())
}

fn remove(_: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let path = dest.join(entry);
    let path_disp = path.display();

    log::debug!("* - Removing: {path_disp}");

    if path.is_dir() {
        log::trace!("remove dir");
        fs::remove_dir(&path)
    } else {
        log::trace!("remove file");
        fs::remove_file(&path)
    }
    .with_context(|| format!("Failed to remove: {path_disp}"))?;

    Ok(())
}

fn copy(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let src = src.join(entry);
    let dest = dest.join(entry);

    let src_disp = src.display();
    let dest_disp = dest.display();

    log::debug!("* + Adding: {src_disp} -> {dest_disp}");

    if let Some(parent) = dest.parent() {
        log::trace!("create dir all");
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory error: {}", parent.display(),))?;
    }

    fs::copy(&src, dest).with_context(|| format!("Failed to copy: {src_disp}"))?;

    Ok(())
}
