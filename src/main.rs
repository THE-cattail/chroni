use std::{
    borrow::Cow,
    cmp::Ordering,
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
use indicatif::{ProgressBar, ProgressStyle};
use sha1::{Digest, Sha1};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut task = Task::parse();
    task.process()?;

    Ok(())
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum OverwriteMode {
    #[value(help = "always overwrite")]
    Always,
    #[value(help = "overwrite when sizes of the source and the destination are different")]
    FastComp,
    #[value(help = "overwrite when hashsum of the source and the destination are different")]
    DeepComp,
    #[value(help = "never overwrite")]
    Never,
}

struct Term {
    term:     console::Term,
    progress: Option<ProgressBar>,
}

impl Term {
    fn act(&self, action: &str, desc: &str) -> Result<()> {
        self.term.write_line(&format!(
            "{: >12} {}",
            console::style(action).bold().cyan(),
            desc
        ))?;
        Ok(())
    }

    fn new_progress_without_bar(&mut self, prefix: impl Into<Cow<'static, str>>) -> Result<()> {
        self.progress = Some(
            ProgressBar::new(0)
                .with_style(ProgressStyle::with_template(
                    "{prefix:>12.bold.cyan} {msg}",
                )?)
                .with_prefix(prefix),
        );
        Ok(())
    }

    fn new_progress(&mut self, len: usize, prefix: impl Into<Cow<'static, str>>) -> Result<()> {
        self.progress = Some(
            ProgressBar::new(len.try_into()?)
                .with_style(
                    ProgressStyle::with_template(
                        "{prefix:>12.bold.cyan} [{bar:27}] {pos}/{len}: {msg}",
                    )?
                    .progress_chars("=> "),
                )
                .with_prefix(prefix),
        );
        Ok(())
    }

    fn progress_msg(&self, msg: impl Into<Cow<'static, str>>) {
        if let Some(progress) = &self.progress {
            progress.set_message(msg);
        }
    }

    fn progress_inc(&self) {
        if let Some(progress) = &self.progress {
            progress.inc(1);
        }
    }

    fn progress_finish(&self) {
        if let Some(progress) = &self.progress {
            progress.finish_and_clear();
        }
    }
}

impl Default for Term {
    fn default() -> Self {
        Self {
            term:     console::Term::stdout(),
            progress: None,
        }
    }
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
        long = "only-newest",
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

    #[clap(skip)]
    term: Term,
}

impl Task {
    fn process(&mut self) -> Result<()> {
        let (src, dest) = self.get_src_dest_paths()?;

        self.term
            .act("Collecting", &format!("src `{}`", src.display()))?;
        let mut include_files = self
            .collect_files(&src)
            .context("Failed to collect include files")?;

        if !self.only_newest.is_empty() {
            self.term.act("Progressing", "only-newest list")?;
            self.keep_only_newest(
                &src,
                &mut include_files,
                &generate_globset(&self.only_newest)
                    .context("Failed to generate globset from only-newest list")?,
            )?;
        }

        self.term
            .act("Collecting", &format!("dest `{}`", dest.display()))?;
        let dest_files = self
            .collect_files(&dest)
            .context("Failed to collect exist files of destination directory")?;

        self.term.act("Generating", "to-do list")?;
        let (add_list, overwrite_list, remove_list) = self
            .generate_to_do_list(
                &src,
                &dest,
                &include_files,
                &dest_files,
                self.overwrite_mode,
            )
            .context("Failed to generate to-do list")?;

        if !self.dry_run {
            self.execute_list("remove", "Removing", &remove_list, remove, &src, &dest)
                .context("Failed to execute remove list")?;
            self.execute_list(
                "overwrite",
                "Overwriting",
                &overwrite_list,
                copy,
                &src,
                &dest,
            )
            .context("Failed to execute overwrite list")?;
            self.execute_list("add", "Adding", &add_list, copy, &src, &dest)
                .context("Failed to execute add list")?;
        }

        self.term.act(
            "Finished",
            &format!("backup `{}` to `{}`", src.display(), dest.display()),
        )?;

        Ok(())
    }

    fn get_src_dest_paths(&self) -> Result<(PathBuf, PathBuf)> {
        if !self.src.is_dir() {
            anyhow::bail!("Source path is not a directory");
        }

        fs::create_dir_all(&self.dest).context("Failed to create destination path")?;

        let src = fs::canonicalize(&self.src)
            .context("Failed to get absolute path of source directory")?;
        let dest = fs::canonicalize(&self.dest)
            .context("Failed to get absolute path of destination directory")?;
        Ok((src, dest))
    }

    fn collect_files(&mut self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut ret = Vec::new();

        self.term.new_progress_without_bar("Walking")?;
        for entry in WalkBuilder::new(path)
            .hidden(false)
            .parents(false)
            .sort_by_file_path(path_cmp)
            .build()
        {
            let entry = entry?;
            let entry_path = entry.path();
            self.term.progress_msg(entry_path.display().to_string());

            let path = entry_path.strip_prefix(path)?;
            ret.push(path.to_owned());
        }
        self.term.progress_finish();

        Ok(ret)
    }

    fn keep_only_newest(
        &mut self,
        src: &Path,
        include_files: &mut Vec<PathBuf>,
        only_newest: &GlobSet,
    ) -> Result<()> {
        let mut m: HashMap<usize, Newest> = HashMap::new();
        let mut to_removes = HashSet::new();

        self.term.new_progress(include_files.len(), "Checking")?;
        for entry in &*include_files {
            self.term.progress_msg(entry.display().to_string());

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

            self.term.progress_inc();
        }
        self.term.progress_finish();

        include_files.retain(|e| !to_removes.contains(e));

        Ok(())
    }

    fn generate_to_do_list(
        &mut self,
        src: &Path,
        dest: &Path,
        include_files: &[PathBuf],
        dest_files: &[PathBuf],
        overwrite_mode: OverwriteMode,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>)> {
        let mut add_list = Vec::new();
        let mut overwrite_list = Vec::new();
        let mut remove_list = Vec::new();

        self.term.new_progress(include_files.len(), "Checking")?;
        for entry in include_files {
            let entry_disp = entry.display();
            self.term.progress_msg(entry_disp.to_string());

            let src = src.join(entry);
            if src.is_dir() {
                continue;
            }

            if !dest_files.contains(entry) {
                log::debug!("+ {entry_disp}");
                add_list.push(entry.clone());
                continue;
            }

            let dest = dest.join(entry);

            if need_overwrite(&src, &dest, overwrite_mode).with_context(|| {
                format!(
                    "Failed to check overwrite: {} -> {}",
                    src.display(),
                    dest.display()
                )
            })? {
                log::debug!("~ {entry_disp}");
            } else {
                log::debug!("^ {entry_disp}");
                overwrite_list.push(entry.clone());
            }
            self.term.progress_inc();
        }
        self.term.progress_finish();

        self.term.new_progress(dest_files.len(), "Checking")?;
        for entry in dest_files {
            let entry_disp = entry.display();
            self.term.progress_msg(entry_disp.to_string());
            if !include_files.contains(entry) {
                log::debug!("- {}", entry_disp);
                remove_list.push(entry.clone());
            }
            self.term.progress_inc();
        }
        self.term.progress_finish();

        Ok((add_list, overwrite_list, remove_list))
    }

    fn execute_list(
        &mut self,
        name: &str,
        action: impl Into<Cow<'static, str>>,
        list: &[PathBuf],
        f: fn(&Path, &Path, &Path) -> Result<()>,
        src: &Path,
        dest: &Path,
    ) -> Result<()> {
        if !list.is_empty() {
            self.term.act("Processing", &format!("{name} list"))?;
            self.term.new_progress(list.len(), action)?;
            for entry in list.iter() {
                self.term.progress_msg(entry.display().to_string());
                if let Err(e) = (f)(src, dest, entry) {
                    log::warn!("Failed to execute {name} task:\n{e:?}");
                };
            }
            self.term.progress_finish();
        }

        Ok(())
    }
}

fn generate_globset(gs: &[Glob]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for g in gs {
        builder.add(g.clone());
        builder.add(Glob::new(&format!("{}/*", g.glob()))?);
    }
    Ok(builder.build()?)
}

fn path_cmp(left: &Path, right: &Path) -> Ordering {
    right.cmp(left)
}

struct Newest {
    entry:   PathBuf,
    created: SystemTime,
}

fn need_overwrite(src: &Path, dest: &Path, mode: OverwriteMode) -> Result<bool> {
    if mode == OverwriteMode::Always {
        return Ok(false);
    }
    if mode == OverwriteMode::Never {
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

    if mode == OverwriteMode::FastComp {
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

fn remove(_: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let path = dest.join(entry);

    if path.is_dir() {
        fs::remove_dir(&path)
    } else {
        fs::remove_file(&path)
    }
    .with_context(|| format!("Failed to remove: {}", path.display()))?;

    Ok(())
}

fn copy(src: &Path, dest: &Path, entry: &Path) -> Result<()> {
    let src = src.join(entry);
    let dest = dest.join(entry);

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory error: {}", parent.display(),))?;
    }

    fs::copy(&src, dest).with_context(|| format!("Failed to copy: {}", src.display()))?;

    Ok(())
}
