//! 工作区的符号索引：函数、方法、类型、模块和顶层常量的名字 → 定义所在的位置。
//!
//! 「转到定义」先在这里查名字，命中时不必遍历目录、逐个读取文件；没有命中或索引还没建好时
//! 退回全文查找（见 [`crate::workspace::project_search`]）。符号的识别与「转到符号」相同，
//! 见 [`crate::navigation::symbols`]。
//!
//! 索引在打开工作区时于后台建立，建好后随目录中的变化增量更新；被忽略的目录、二进制文件、
//! 过大或不是代码的文件不在索引中。
//!
//! 建好的索引按工作区存到缓存文件中（见 [`SymbolIndex::ensure`] 的 `cache` 参数）。下次打开同一
//! 工作区时先读缓存，界面立刻可以查询；随后在后台比较各文件的修改时间和大小，只重新读取变了的
//! 文件，再写回缓存。不逐个文件计算内容摘要：读一遍所有文件与重新建立索引一样慢，修改时间和
//! 大小已经足够发现变化（文件系统时间精度不够时按「比缓存新」处理，宁可多读一个文件）。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ropey::Rope;
use winc_backend::fs::Stamp;

use crate::background;
use crate::navigation::symbols::{self, SymbolKind};
use crate::workspace::tree::{folder_prefixes, is_listed_file, labelled_path, walker_skipping};
use crate::workspace::watch::Changes;

/// 索引收录的最大文件数（个）。
const MAX_FILES: usize = 100_000;
/// 收录的文件大小上限（字节）：更大的文件不读、不收录。
const MAX_FILE_BYTES: u64 = 1 << 20;
/// 索引的位置数上限（处），达到后不再收录。
const MAX_PLACES: usize = 1_000_000;
/// 这种语言（id）的文件是否会进入符号索引：配置、文档等没有代码定义的语言不收录。
pub fn indexable_language(id: &str) -> bool {
    !NOT_INDEXED.contains(&id)
}

/// 一次变化超过这么多个路径时整个重新建立索引。
const MAX_PATCH: usize = 512;
/// 依赖目录：设置中「忽略依赖目录」开启时跳过。
const DEPENDENCY_DIRS: &[&str] = &[
    "node_modules",
    "bower_components",
    "vendor",
    ".venv",
    "venv",
    "Pods",
];
/// 构建输出目录：设置中「忽略构建输出目录」开启时跳过。
const BUILD_DIRS: &[&str] = &[
    "dist", "build", "out", "target", "coverage", ".next", ".nuxt", ".output",
];
/// 缓存最多保存的文件数和位置数；更大的工作区仍建内存索引，但不写缓存。
const CACHE_MAX_FILES: usize = 50_000;
const CACHE_MAX_PLACES: usize = 300_000;
/// 缓存文件的格式版本；格式变化时旧缓存直接作废。
const CACHE_VERSION: u32 = 2;
/// 读取缓存文件的大小上限（字节）。
const CACHE_MAX_BYTES: u64 = 64 << 20;
/// 判断「文件在写缓存之后改过」的余量（秒）：文件系统时间精度不高时避免漏掉同一秒内的修改。
const STAMP_MARGIN: i64 = 1;

/// 不收录的语言：其中没有代码定义，或定义由「转到符号」以外的写法识别（如配置文件的小节）。
const NOT_INDEXED: &[&str] = &[
    "text",
    "json",
    "html",
    "xml",
    "svg",
    "css",
    "scss",
    "sass",
    "less",
    "wxss",
    "dockerfile",
    "dotenv",
    "ignore",
    "markdown",
    "toml",
    "ini",
    "properties",
    "yaml",
];

/// 建立索引时收录哪些文件，来自设置中的「符号索引」一组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexOptions {
    /// 不收录的语言 id（如 `go`）；空时收录全部代码文件。
    pub exclude: Vec<String>,
    /// 跳过依赖目录（`node_modules`、`vendor` 等）。
    pub skip_dependencies: bool,
    /// 跳过构建输出目录（`dist`、`build` 等）。
    pub skip_build: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self::everything()
    }
}

impl IndexOptions {
    /// 收录全部代码文件、跳过依赖和构建输出目录：设置中未改动时的默认值。
    pub fn everything() -> IndexOptions {
        IndexOptions {
            exclude: Vec::new(),
            skip_dependencies: true,
            skip_build: true,
        }
    }

    /// 遍历目录时跳过的目录名。
    fn skipped_dirs(&self) -> Vec<&'static str> {
        let mut skipped = Vec::new();
        if self.skip_dependencies {
            skipped.extend_from_slice(DEPENDENCY_DIRS);
        }
        if self.skip_build {
            skipped.extend_from_slice(BUILD_DIRS);
        }
        skipped
    }

    /// 是否跳过相对路径 `relative`：某一级目录在这个集合中。
    fn skipped_path(&self, relative: &str) -> bool {
        let skipped = self.skipped_dirs();
        relative.split('/').any(|part| skipped.contains(&part))
    }

    /// 是否收录语言 `id` 的文件。
    fn indexes_language(&self, id: &str) -> bool {
        !self.exclude.iter().any(|excluded| excluded == id)
    }
}

/// 索引中的一处定义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolPlace {
    /// 文件的绝对路径。
    pub path: PathBuf,
    /// 相对于工作区根目录的路径，附加目录带各自的前缀，在列表中显示。
    pub relative: String,
    /// 行号（从 0 起）。
    pub line: usize,
    /// 名字在该行的列（字符偏移，从 0 起）。
    pub col: usize,
    /// 定义的种类。
    pub kind: SymbolKind,
}

/// 索引当前的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPhase {
    /// 正在读取缓存。
    Loading,
    /// 正在从头建立（没有可用的缓存）。
    Building,
    /// 已从缓存读出，正在比较文件、重新读取变了的文件。
    Refreshing,
}

/// 索引的进度，供状态栏显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatus {
    /// 当前的阶段。
    pub phase: IndexPhase,
    /// 已处理的文件数。
    pub done: usize,
    /// 要处理的文件数；还不知道时为 0（正在扫描目录）。
    pub total: usize,
}

/// 后台任务的进度，界面按 [`SymbolIndex::status`] 读取。
#[derive(Default)]
struct Progress {
    phase: AtomicU8,
    done: AtomicU32,
    total: AtomicU32,
    busy: AtomicBool,
}

impl Progress {
    fn begin(&self, phase: IndexPhase) {
        self.phase.store(phase_code(phase), Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
        self.busy.store(true, Ordering::Release);
    }

    fn total(&self, total: usize) {
        self.total
            .store(total.min(u32::MAX as usize) as u32, Ordering::Relaxed);
    }

    fn step(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(&self) {
        self.busy.store(false, Ordering::Release);
    }

    fn status(&self) -> Option<IndexStatus> {
        if !self.busy.load(Ordering::Acquire) {
            return None;
        }
        Some(IndexStatus {
            phase: phase_from(self.phase.load(Ordering::Relaxed)),
            done: self.done.load(Ordering::Relaxed) as usize,
            total: self.total.load(Ordering::Relaxed) as usize,
        })
    }
}

fn phase_code(phase: IndexPhase) -> u8 {
    match phase {
        IndexPhase::Loading => 0,
        IndexPhase::Building => 1,
        IndexPhase::Refreshing => 2,
    }
}

fn phase_from(code: u8) -> IndexPhase {
    match code {
        2 => IndexPhase::Refreshing,
        1 => IndexPhase::Building,
        _ => IndexPhase::Loading,
    }
}

/// 倒排表中的一个位置。
#[derive(Debug, Clone, Copy)]
struct Posting {
    /// 文件在 `Table::files` 中的编号。
    file: u32,
    line: u32,
    col: u32,
    kind: SymbolKind,
}

/// 一个文件中收录的符号（名字和位置）。
struct Indexed {
    name: String,
    line: u32,
    col: u32,
    kind: SymbolKind,
}

/// 文件的变化依据：修改时间和大小，与缓存中记下的比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<Stamp>,
    len: u64,
}

impl FileStamp {
    fn of(metadata: &winc_backend::fs::Metadata) -> FileStamp {
        FileStamp {
            modified: metadata.modified,
            len: metadata.len,
        }
    }

    /// 是否比时刻 `saved` 更晚（或就在同一秒内）：此时缓存可能没包含它的改动。
    fn newer_than(&self, saved: i64) -> bool {
        self.modified
            .is_some_and(|stamp| stamp.secs >= saved.saturating_sub(STAMP_MARGIN))
    }
}

/// 一个文件及其中的符号。
struct FileEntry {
    path: PathBuf,
    relative: String,
    /// 读取时的修改时间和大小；读不到修改时间时为 `None`。
    stamp: Option<FileStamp>,
    symbols: Vec<Indexed>,
}

/// 遍历到的一个文件：绝对路径、相对路径和当时的变化依据。
#[derive(Debug, Clone)]
struct ListedFile {
    path: PathBuf,
    relative: String,
    stamp: Option<FileStamp>,
}

/// 索引表：名字到位置的倒排表，以及各文件按相对路径记下的位置（增量更新时据此去掉旧的）。
#[derive(Default)]
struct Table {
    postings: HashMap<String, Vec<Posting>>,
    files: HashMap<u32, FileEntry>,
    ids: HashMap<String, u32>,
    next: u32,
    places: usize,
    /// 是否已经可以查询；建立过程中和清空后为 `false`。
    ready: bool,
    /// 创建这张表时的工作区代次：旧代次的后台任务完成后不会覆盖它。
    generation: u64,
}

impl Table {
    /// 记入一个文件及其中的符号。
    fn add(&mut self, entry: FileEntry) {
        let id = self.next;
        self.next += 1;
        for symbol in &entry.symbols {
            if self.places >= MAX_PLACES {
                break;
            }
            self.postings
                .entry(symbol.name.clone())
                .or_default()
                .push(Posting {
                    file: id,
                    line: symbol.line,
                    col: symbol.col,
                    kind: symbol.kind,
                });
            self.places += 1;
        }
        self.ids.insert(entry.relative.clone(), id);
        self.files.insert(id, entry);
    }

    /// 去掉相对路径为 `relative` 的文件和它的位置。
    fn remove(&mut self, relative: &str) {
        let Some(id) = self.ids.remove(relative) else {
            return;
        };
        let Some(entry) = self.files.remove(&id) else {
            return;
        };
        for symbol in &entry.symbols {
            if let Some(list) = self.postings.get_mut(&symbol.name) {
                list.retain(|posting| posting.file != id);
                if list.is_empty() {
                    self.postings.remove(&symbol.name);
                }
            }
            self.places = self.places.saturating_sub(1);
        }
    }

    /// 去掉相对路径 `prefix` 本身和它之下的各文件。
    fn remove_under(&mut self, prefix: &str) {
        let nested = |relative: &str| {
            relative == prefix
                || relative
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let paths: Vec<String> = self
            .ids
            .keys()
            .filter(|relative| nested(relative))
            .cloned()
            .collect();
        for path in paths {
            self.remove(&path);
        }
    }

    /// 按一批编辑更新表。
    fn apply(&mut self, edits: Vec<Edit>) {
        for edit in edits {
            match edit {
                Edit::Remove(relative) => {
                    self.remove(&relative);
                    self.remove_under(&relative);
                }
                Edit::Set(entry) => {
                    self.remove(&entry.relative);
                    self.add(entry);
                }
            }
        }
    }
}

/// 增量更新中的一项：去掉一个路径（文件或目录），或用新内容替换一个文件。
enum Edit {
    Remove(String),
    Set(FileEntry),
}

/// 工作区的符号索引。
///
/// [`Self::ensure`] 在打开工作区或在第一次需要时建立索引，先从缓存读出可用的旧表，再在后台
/// 重新读取变化的文件；此后 [`Self::lookup`] 直接查表，[`Self::apply`] 按目录中的变化增量更新。
#[derive(Default)]
pub struct SymbolIndex {
    root: Option<PathBuf>,
    extra: Vec<PathBuf>,
    table: Arc<RwLock<Table>>,
    /// 最新的工作区代次，后台任务完成后据此判断结果是否已经过时。
    latest: Arc<AtomicU64>,
    generation: u64,
    /// 正在建立索引的后台任务的停止标志。
    cancel: Option<Arc<AtomicBool>>,
    /// 等待增量更新的路径，以及在后台更新它们是否正在进行。
    pending: Arc<Mutex<Vec<PathBuf>>>,
    patching: Arc<AtomicBool>,
    /// 这个工作区的缓存文件；没有缓存时不读写。
    cache: Option<PathBuf>,
    /// 收录哪些文件；变化时重新建立。
    options: IndexOptions,
    /// 当前后台任务的进度；每次重新建立时换成新的，旧任务的结果不再显示。
    progress: Arc<Progress>,
}

impl SymbolIndex {
    /// 状态栏检查索引进度的间隔。
    pub const PROGRESS_POLL: Duration = Duration::from_millis(200);

    /// 确保已按 `options` 为 `root`（和附加的目录 `extra`）建立索引；换了工作区或选项时重新
    /// 建立。
    ///
    /// `cache` 是缓存所在的目录（按工作区在其中各有一个文件）。建立和更新都在后台进行；
    /// 没有事件循环（测试）时不能调用，见 [`Self::collect_now`]。
    pub fn ensure(
        &mut self,
        root: &Path,
        extra: &[PathBuf],
        options: IndexOptions,
        cache: Option<PathBuf>,
    ) {
        if self.covers(root, extra, &options) {
            return;
        }
        self.cache = cache.map(|dir| cache_file(&dir, root, extra));
        self.start(root, extra, options);
    }

    /// 是否正是按 `options` 为 `root` 和 `extra` 建立的索引（无论是否已经建立完成）。
    pub fn covers(&self, root: &Path, extra: &[PathBuf], options: &IndexOptions) -> bool {
        self.root.as_deref() == Some(root) && self.extra == extra && self.options == *options
    }

    /// 索引是否已经可以查询（从缓存读出或建立完成）。
    pub fn is_ready(&self) -> bool {
        self.read().ready
    }

    /// 返回后台任务的进度；没有任务在跑时返回 `None`。
    pub fn status(&self) -> Option<IndexStatus> {
        self.progress.status()
    }

    /// 查名字 `name` 的定义所在的位置。
    ///
    /// 索引还不可查询时返回 `None`；可查询而没有这个名字时返回空列表（调用方据此决定是否退回
    /// 全文查找，非代码文件和没收录的大文件不在索引中）。
    pub fn lookup(&self, name: &str) -> Option<Vec<SymbolPlace>> {
        let table = self.read();
        if !table.ready {
            return None;
        }
        let Some(postings) = table.postings.get(name) else {
            return Some(Vec::new());
        };
        let places = postings
            .iter()
            .filter_map(|posting| {
                let file = table.files.get(&posting.file)?;
                Some(SymbolPlace {
                    path: file.path.clone(),
                    relative: file.relative.clone(),
                    line: posting.line as usize,
                    col: posting.col as usize,
                    kind: posting.kind,
                })
            })
            .collect();
        Some(places)
    }

    /// 按目录中的变化更新索引：变化涉及的文件重新读取并提取符号，删除的去掉。
    ///
    /// 后台正在建立、校正索引时不处理（它会读到最新内容）；变化太多或系统丢失通知时整个重新
    /// 建立。增量更新不改写缓存：下次打开时按修改时间重新读取变了的文件。
    pub fn apply(&mut self, changes: &Changes) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.read().ready || self.progress.status().is_some() {
            // 后台任务还没读完缓存时交给它；建立、校正期间也由它读到最新内容。
            if self.progress.status().is_some() {
                return;
            }
        } else {
            return;
        }
        if changes.rescan || changes.paths.len() > MAX_PATCH {
            self.rebuild();
            return;
        }
        if changes.paths.is_empty() {
            return;
        }

        let overflow = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            pending.extend(changes.paths.iter().cloned());
            let overflow = pending.len() > MAX_PATCH;
            if overflow {
                pending.clear();
            }
            overflow
        };
        if overflow {
            self.rebuild();
            return;
        }
        if self.patching.swap(true, Ordering::AcqRel) {
            return;
        }

        let generation = self.generation;
        let extra = self.extra.clone();
        let options = self.options.clone();
        let table = Arc::clone(&self.table);
        let latest = Arc::clone(&self.latest);
        let pending = Arc::clone(&self.pending);
        let patching = Arc::clone(&self.patching);
        background::spawn_blocking(move || {
            loop {
                let batch = {
                    let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                    // 换了工作区：这一批变化属于旧的目录，丢掉；随后由新的建立读到最新内容。
                    if latest.load(Ordering::Relaxed) != generation {
                        pending.clear();
                        patching.store(false, Ordering::Release);
                        break;
                    }
                    let batch = std::mem::take(&mut *pending);
                    if batch.is_empty() {
                        patching.store(false, Ordering::Release);
                        break;
                    }
                    batch
                };
                let edits = edits(&root, &extra, &options, &batch);
                if latest.load(Ordering::Relaxed) != generation {
                    patching.store(false, Ordering::Release);
                    break;
                }
                let mut table = table.write().unwrap_or_else(PoisonError::into_inner);
                if table.generation != generation {
                    patching.store(false, Ordering::Release);
                    break;
                }
                table.apply(edits);
            }
        });
    }

    /// 清空索引并停止后台任务（关闭目录时）。缓存文件保留给下次打开。
    pub fn clear(&mut self) {
        self.cancel();
        self.root = None;
        self.extra.clear();
        self.cache = None;
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.patching.store(false, Ordering::Release);
        self.generation += 1;
        self.latest.store(self.generation, Ordering::Relaxed);
        let generation = self.generation;
        self.progress = Arc::new(Progress::default());
        *self.write() = Table {
            generation,
            ..Default::default()
        };
    }

    /// 在当前线程同步建立索引（测试用，不读写缓存）。
    pub fn collect_now(root: &Path, extra: &[PathBuf]) -> SymbolIndex {
        Self::build_now(root, extra, IndexOptions::default(), None)
    }

    /// 在当前线程同步建立索引（测试用）；`cache` 是缓存文件，为 `None` 时不读写缓存。
    fn build_now(
        root: &Path,
        extra: &[PathBuf],
        options: IndexOptions,
        cache: Option<&Path>,
    ) -> SymbolIndex {
        let table = Arc::new(RwLock::new(Table::default()));
        let latest = Arc::new(AtomicU64::new(1));
        let progress = Arc::new(Progress::default());
        let cancel = AtomicBool::new(false);
        run(
            root, extra, &options, cache, 1, &cancel, &latest, &progress, &table,
        );
        SymbolIndex {
            root: Some(root.to_path_buf()),
            extra: extra.to_vec(),
            table,
            latest,
            generation: 1,
            cache: cache.map(Path::to_path_buf),
            options,
            progress,
            ..Default::default()
        }
    }

    /// 重新建立索引；旧表在重新建立完成之前仍可查询。
    fn rebuild(&mut self) {
        if let Some(root) = self.root.clone() {
            let extra = self.extra.clone();
            let options = self.options.clone();
            self.start(&root, &extra, options);
        }
    }

    /// 在后台为 `root` 和 `extra` 建立索引。
    fn start(&mut self, root: &Path, extra: &[PathBuf], options: IndexOptions) {
        self.cancel();
        self.root = Some(root.to_path_buf());
        self.extra = extra.to_vec();
        self.options = options.clone();
        self.generation += 1;
        // 旧的更新任务不会再应用结果（代次不符），标志留给新的更新使用。
        self.patching.store(false, Ordering::Release);
        let generation = self.generation;
        self.latest.store(generation, Ordering::Relaxed);
        // 旧任务只更新它自己的进度，界面只看这一次的。
        self.progress = Arc::new(Progress::default());

        if winc_backend::is_remote(root)
            || extra.iter().any(|folder| winc_backend::is_remote(folder))
        {
            // 远端的目录要等待网络，全文查找由远端服务进程处理，不建索引。
            *self.write() = Table {
                generation,
                ..Default::default()
            };
            return;
        }

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(Arc::clone(&cancel));
        let table = Arc::clone(&self.table);
        let latest = Arc::clone(&self.latest);
        let progress = Arc::clone(&self.progress);
        let cache = self.cache.clone();
        let (root, extra) = (root.to_path_buf(), extra.to_vec());
        background::spawn_blocking(move || {
            run(
                &root,
                &extra,
                &options,
                cache.as_deref(),
                generation,
                &cancel,
                &latest,
                &progress,
                &table,
            );
            progress.finish();
        });
    }

    /// 让正在进行的建立、更新尽快结束。
    fn cancel(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Table> {
        self.table.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Table> {
        self.table.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 后台任务的主体：先读缓存并立即发布，再在后台校正；没有缓存时从头建立。
#[allow(clippy::too_many_arguments)]
fn run(
    root: &Path,
    extra: &[PathBuf],
    options: &IndexOptions,
    cache: Option<&Path>,
    generation: u64,
    cancel: &AtomicBool,
    latest: &AtomicU64,
    progress: &Progress,
    table: &Arc<RwLock<Table>>,
) {
    let stale = || cancel.load(Ordering::Relaxed) || latest.load(Ordering::Relaxed) != generation;

    if let Some(path) = cache {
        progress.begin(IndexPhase::Loading);
        if let Some((mut loaded, saved)) = cache_load(path, root, extra, options) {
            loaded.generation = generation;
            loaded.ready = true;
            if stale() {
                progress.finish();
                return;
            }
            *table.write().unwrap_or_else(PoisonError::into_inner) = loaded;
            progress.begin(IndexPhase::Refreshing);
            if refresh(
                root, extra, options, saved, generation, cancel, latest, progress, table,
            ) {
                save(root, extra, options, path, table);
            }
            progress.finish();
            return;
        }
    }

    progress.begin(IndexPhase::Building);
    let built = build(root, extra, options, cancel, generation, progress);
    if stale() {
        progress.finish();
        return;
    }
    *table.write().unwrap_or_else(PoisonError::into_inner) = built;
    if let Some(path) = cache {
        save(root, extra, options, path, table);
    }
    progress.finish();
}

/// 从缓存表出发，比较当前文件，只重新读取变了的文件；返回是否完成（换工作区时提前结束）。
#[allow(clippy::too_many_arguments)]
fn refresh(
    root: &Path,
    extra: &[PathBuf],
    options: &IndexOptions,
    saved: i64,
    generation: u64,
    cancel: &AtomicBool,
    latest: &AtomicU64,
    progress: &Progress,
    table: &Arc<RwLock<Table>>,
) -> bool {
    let labelled = labels(root, extra);
    let listed = list_files(root, &labelled, options, cancel);
    if cancel.load(Ordering::Relaxed) || latest.load(Ordering::Relaxed) != generation {
        return false;
    }
    let current: HashSet<String> = listed.iter().map(|file| file.relative.clone()).collect();
    let removed: Vec<String> = {
        let table = table.read().unwrap_or_else(PoisonError::into_inner);
        table
            .ids
            .keys()
            .filter(|relative| !current.contains(*relative))
            .cloned()
            .collect()
    };
    let changed: Vec<ListedFile> = {
        let table = table.read().unwrap_or_else(PoisonError::into_inner);
        listed
            .into_iter()
            .filter(|file| match table.ids.get(&file.relative) {
                None => true,
                Some(id) => table.files.get(id).is_none_or(|entry| {
                    entry.stamp != file.stamp
                        || file.stamp.is_some_and(|stamp| stamp.newer_than(saved))
                }),
            })
            .collect()
    };
    {
        let mut table = table.write().unwrap_or_else(PoisonError::into_inner);
        for relative in removed {
            table.remove(&relative);
        }
    }
    if changed.is_empty() {
        return true;
    }

    progress.total(changed.len());
    let found = extract_many(&changed, options, cancel, progress);
    if cancel.load(Ordering::Relaxed) || latest.load(Ordering::Relaxed) != generation {
        return false;
    }
    let mut entries: HashMap<String, FileEntry> = changed
        .iter()
        .zip(found)
        .filter_map(|(file, entry)| Some((file.relative.clone(), entry?)))
        .collect();
    let mut table = table.write().unwrap_or_else(PoisonError::into_inner);
    if table.generation != generation {
        return false;
    }
    for file in &changed {
        table.remove(&file.relative);
        if let Some(entry) = entries.remove(&file.relative) {
            table.add(entry);
        }
    }
    true
}

/// 建立整张索引表。`generation` 记入表中，旧代次的后台任务不会覆盖它。
fn build(
    root: &Path,
    extra: &[PathBuf],
    options: &IndexOptions,
    cancel: &AtomicBool,
    generation: u64,
    progress: &Progress,
) -> Table {
    let labelled = labels(root, extra);
    let files = list_files(root, &labelled, options, cancel);
    if cancel.load(Ordering::Relaxed) {
        return Table {
            generation,
            ..Default::default()
        };
    }
    progress.total(files.len());
    let found = extract_many(&files, options, cancel, progress);
    let mut table = Table {
        generation,
        ready: true,
        ..Default::default()
    };
    for entry in found.into_iter().flatten() {
        if table.places >= MAX_PLACES {
            break;
        }
        table.add(entry);
    }
    table
}

/// 返回 `root` 和 `extra` 在相对路径中的前缀和目录。
fn labels(root: &Path, extra: &[PathBuf]) -> Vec<(String, PathBuf)> {
    folder_prefixes(root, extra)
        .into_iter()
        .zip(extra.iter().cloned())
        .collect()
}

/// 列出要收录的文件：绝对路径、相对路径，以及读取时的修改时间和大小。
///
/// 过大的文件不列出（读取时必然跳过）；`labelled` 是附加目录的前缀和目录。
fn list_files(
    root: &Path,
    labelled: &[(String, PathBuf)],
    options: &IndexOptions,
    cancel: &AtomicBool,
) -> Vec<ListedFile> {
    let skipped = options.skipped_dirs();
    let dirs =
        std::iter::once(root.to_path_buf()).chain(labelled.iter().map(|(_, dir)| dir.clone()));
    let mut seen: HashSet<String> = HashSet::new();
    let mut files = Vec::new();
    for dir in dirs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        for entry in walker_skipping(&dir, &skipped).build().flatten() {
            let Some(kind) = entry.file_type() else {
                continue;
            };
            if !is_listed_file(kind, entry.path()) {
                continue;
            }
            let path = entry.into_path();
            let Ok(metadata) = winc_backend::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len > MAX_FILE_BYTES {
                continue;
            }
            let relative = labelled_path(root, labelled, &path);
            let nested = |part: &str| skipped.contains(&part);
            if relative.split('/').any(nested) || !seen.insert(relative.clone()) {
                continue;
            }
            files.push(ListedFile {
                path,
                relative,
                stamp: Some(FileStamp::of(&metadata)),
            });
            if files.len() >= MAX_FILES {
                return files;
            }
        }
    }
    files
}

/// 并行提取各文件的符号，结果与 `files` 一一对应（不收录的文件为 `None`），每处理一个文件
/// 就累加一次进度。
fn extract_many(
    files: &[ListedFile],
    options: &IndexOptions,
    cancel: &AtomicBool,
    progress: &Progress,
) -> Vec<Option<FileEntry>> {
    let slots = Mutex::new((0..files.len()).map(|_| None).collect::<Vec<_>>());
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .min(winc_search::MAX_THREADS);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(index) else {
                        break;
                    };
                    let found = extract(file, options);
                    let mut slots = slots.lock().unwrap_or_else(PoisonError::into_inner);
                    slots[index] = found;
                    drop(slots);
                    progress.step();
                }
            });
        }
    });
    slots.into_inner().unwrap_or_else(PoisonError::into_inner)
}

/// 读取一个文件并提取其中收录的符号；不是代码、语言被排除、读不到或没有符号时返回 `None`。
fn extract(file: &ListedFile, options: &IndexOptions) -> Option<FileEntry> {
    let bytes = winc_backend::fs::read(&file.path).ok()?;
    if winc_search::looks_binary(&bytes) {
        return None;
    }
    let (text, _) = winc_search::text::decode_unknown(&bytes);
    if text.is_empty() {
        return None;
    }
    let rope = Rope::from_str(&text);
    let first_line = rope
        .get_line(0)
        .map_or_else(String::new, |line| line.to_string());
    let name = file.path.file_name()?.to_string_lossy();
    let language = winc_syntax::detect_with_first_line(&name, &first_line);
    if NOT_INDEXED.contains(&language.id) || !options.indexes_language(language.id) {
        return None;
    }
    let symbols: Vec<Indexed> = symbols::symbols(&rope, language)
        .into_iter()
        .filter(|symbol| indexed(symbol.kind))
        .map(|symbol| Indexed {
            name: symbol.name,
            line: symbol.line as u32,
            col: symbol.col as u32,
            kind: symbol.kind,
        })
        .collect();
    (!symbols.is_empty()).then(|| FileEntry {
        path: file.path.clone(),
        relative: file.relative.clone(),
        stamp: file.stamp,
        symbols,
    })
}

/// 是否收录这种符号：用户要找的方法、类型、模块和常量；标题和配置文件的小节不收录。
fn indexed(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::Type
            | SymbolKind::Module
            | SymbolKind::Variable
    )
}

/// 按一批变化的路径算出要应用到表上的编辑。
fn edits(root: &Path, extra: &[PathBuf], options: &IndexOptions, paths: &[PathBuf]) -> Vec<Edit> {
    let labelled = labels(root, extra);
    let mut seen = HashSet::new();
    let mut edits = Vec::new();
    for path in paths {
        if !seen.insert(path.clone()) {
            continue;
        }
        let relative = labelled_path(root, &labelled, path);
        if options.skipped_path(&relative) {
            edits.push(Edit::Remove(relative));
            continue;
        }
        match winc_backend::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() && metadata.len <= MAX_FILE_BYTES => {
                let file = ListedFile {
                    path: path.clone(),
                    relative,
                    stamp: Some(FileStamp::of(&metadata)),
                };
                match extract(&file, options) {
                    Some(entry) => edits.push(Edit::Set(entry)),
                    None => edits.push(Edit::Remove(file.relative)),
                }
            }
            // 删除或目录：去掉它和它下面的文件；目录中新出现的文件等下次变化或重新建立时收录。
            _ => edits.push(Edit::Remove(relative)),
        }
    }
    edits
}

/// 返回工作区在缓存目录中的文件：按根目录和附加目录算出的名字，同一工作区始终相同。
fn cache_file(dir: &Path, root: &Path, extra: &[PathBuf]) -> PathBuf {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in std::iter::once(root.to_path_buf()).chain(extra.iter().cloned()) {
        for &byte in part.to_string_lossy().as_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    dir.join(format!("{hash:016x}.bin"))
}

/// 返回当前的 Unix 秒数。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs().min(i64::MAX as u64) as i64)
}

/// 把表写回缓存；表太大时不写。写入是原子的：先写临时文件再改名。
// 缓存文件总在本机的状态目录中，与工作区是否在远端无关。
#[allow(clippy::disallowed_methods, reason = "缓存文件位于本机的状态目录")]
fn save(
    root: &Path,
    extra: &[PathBuf],
    options: &IndexOptions,
    path: &Path,
    table: &Arc<RwLock<Table>>,
) {
    let bytes = {
        let table = table.read().unwrap_or_else(PoisonError::into_inner);
        if table.files.len() > CACHE_MAX_FILES || table.places > CACHE_MAX_PLACES {
            return;
        }
        encode(root, extra, options, &table)
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let temp = path.with_extension("tmp");
    if fs::write(&temp, &bytes).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    // Windows 上改名不能覆盖已有的文件。
    let _ = fs::remove_file(path);
    if fs::rename(&temp, path).is_err() {
        let _ = fs::remove_file(&temp);
    }
}

/// 缓存文件的魔数和版本。
const MAGIC: &[u8; 8] = b"WINCSYM1";

/// 把一个表编码为缓存文件的内容。
fn encode(root: &Path, extra: &[PathBuf], options: &IndexOptions, table: &Table) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(64 + table.files.len() * 64);
    bytes.extend_from_slice(MAGIC);
    push_u32(&mut bytes, CACHE_VERSION);
    push_i64(&mut bytes, now_secs());
    push_str(&mut bytes, &root.to_string_lossy());
    push_u32(&mut bytes, extra.len() as u32);
    for folder in extra {
        push_str(&mut bytes, &folder.to_string_lossy());
    }
    // 收录哪些文件的选项：改了选项（语言、跳过的目录）时旧缓存不能再用。
    push_u32(&mut bytes, options.exclude.len() as u32);
    for language in &options.exclude {
        push_str(&mut bytes, language);
    }
    bytes.push(u8::from(options.skip_dependencies));
    bytes.push(u8::from(options.skip_build));
    push_u32(&mut bytes, table.files.len() as u32);
    // 按相对路径排序，缓存内容稳定，便于比较和排查。
    let mut files: Vec<&FileEntry> = table.files.values().collect();
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    for file in files {
        push_str(&mut bytes, &file.relative);
        push_str(&mut bytes, &file.path.to_string_lossy());
        match file.stamp {
            Some(stamp) => {
                bytes.push(1);
                let modified = stamp.modified.unwrap_or(Stamp { secs: 0, nanos: 0 });
                push_i64(&mut bytes, modified.secs);
                push_u32(&mut bytes, modified.nanos);
                push_u64(&mut bytes, stamp.len);
            }
            None => {
                bytes.push(0);
                push_u64(&mut bytes, 0);
            }
        }
        push_u32(&mut bytes, file.symbols.len() as u32);
        for symbol in &file.symbols {
            push_str(&mut bytes, &symbol.name);
            push_u32(&mut bytes, symbol.line);
            push_u32(&mut bytes, symbol.col);
            bytes.push(kind_code(symbol.kind));
        }
    }
    bytes
}

/// 读缓存文件；格式不符、属于别的工作区或内容不全时返回 `None`。
// 缓存文件总在本机的状态目录中，与工作区是否在远端无关。
#[allow(clippy::disallowed_methods, reason = "缓存文件位于本机的状态目录")]
fn cache_load(
    path: &Path,
    root: &Path,
    extra: &[PathBuf],
    options: &IndexOptions,
) -> Option<(Table, i64)> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > CACHE_MAX_BYTES {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let mut cursor = Cursor::new(&bytes);
    if cursor.take(MAGIC.len())? != MAGIC || cursor.u32()? != CACHE_VERSION {
        return None;
    }
    let saved = cursor.i64()?;
    if cursor.string()? != root.to_string_lossy() {
        return None;
    }
    let count = cursor.u32()? as usize;
    if count > 64 {
        return None;
    }
    let mut folders = Vec::with_capacity(count);
    for _ in 0..count {
        folders.push(PathBuf::from(cursor.string()?));
    }
    if folders != extra {
        return None;
    }
    let excluded = cursor.u32()? as usize;
    if excluded > 64 {
        return None;
    }
    let mut exclude = Vec::with_capacity(excluded);
    for _ in 0..excluded {
        exclude.push(cursor.string()?);
    }
    let skip_dependencies = match cursor.u8()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let skip_build = match cursor.u8()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let cached_options = IndexOptions {
        exclude,
        skip_dependencies,
        skip_build,
    };
    if cached_options != *options {
        return None;
    }
    let file_count = cursor.u32()? as usize;
    if file_count > CACHE_MAX_FILES {
        return None;
    }
    let mut table = Table {
        ready: true,
        ..Default::default()
    };
    let mut places = 0usize;
    for _ in 0..file_count {
        let relative = cursor.string()?;
        let path = PathBuf::from(cursor.string()?);
        let stamp = match cursor.u8()? {
            1 => {
                let secs = cursor.i64()?;
                let nanos = cursor.u32()?;
                let len = cursor.u64()?;
                Some(FileStamp {
                    modified: Some(Stamp { secs, nanos }),
                    len,
                })
            }
            0 => {
                let _ = cursor.u64()?;
                None
            }
            _ => return None,
        };
        let symbol_count = cursor.u32()? as usize;
        places = places.checked_add(symbol_count)?;
        if places > CACHE_MAX_PLACES {
            return None;
        }
        let mut symbols = Vec::with_capacity(symbol_count);
        for _ in 0..symbol_count {
            let name = cursor.string()?;
            let line = cursor.u32()?;
            let col = cursor.u32()?;
            let kind = kind_from(cursor.u8()?)?;
            symbols.push(Indexed {
                name,
                line,
                col,
                kind,
            });
        }
        table.add(FileEntry {
            path,
            relative,
            stamp,
            symbols,
        });
    }
    cursor.at_end().then_some((table, saved))
}

fn kind_code(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Type => 2,
        SymbolKind::Module => 3,
        SymbolKind::Variable => 4,
        SymbolKind::Heading => 5,
        SymbolKind::Section => 6,
    }
}

fn kind_from(code: u8) -> Option<SymbolKind> {
    Some(match code {
        0 => SymbolKind::Function,
        1 => SymbolKind::Method,
        2 => SymbolKind::Type,
        3 => SymbolKind::Module,
        4 => SymbolKind::Variable,
        5 => SymbolKind::Heading,
        6 => SymbolKind::Section,
        _ => return None,
    })
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_i64(bytes: &mut Vec<u8>, value: i64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_str(bytes: &mut Vec<u8>, value: &str) {
    push_u32(bytes, value.len() as u32);
    bytes.extend_from_slice(value.as_bytes());
}

/// 读取缓存内容的位置和越界检查。
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|slice| slice[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let slice = self.take(4)?;
        Some(u32::from_le_bytes(slice.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        let slice = self.take(8)?;
        Some(u64::from_le_bytes(slice.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        let slice = self.take(8)?;
        Some(i64::from_le_bytes(slice.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).ok()
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("winc-symbol-{name}-{}", std::process::id()))
    }

    #[test]
    fn indexes_definitions_by_name() {
        let dir = temp_dir("index");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("staff.py"),
            "class Staff:\n    def greet(self):\n        return 1\n\nMAX_SIZE = 3\n",
        )
        .unwrap();
        std::fs::write(dir.join("readme.md"), "# Staff\n").unwrap();

        let index = SymbolIndex::collect_now(&dir, &[]);
        assert!(index.is_ready());

        let staff = index.lookup("Staff").unwrap();
        assert_eq!(staff.len(), 1);
        assert_eq!(staff[0].relative, "staff.py");
        assert_eq!((staff[0].line, staff[0].col), (0, 6));
        assert_eq!(staff[0].kind, SymbolKind::Type);

        let greet = index.lookup("greet").unwrap();
        assert_eq!((greet[0].line, greet[0].col), (1, 8));

        let constant = index.lookup("MAX_SIZE").unwrap();
        assert_eq!(constant[0].line, 4);
        assert_eq!(constant[0].kind, SymbolKind::Variable);

        // 不在索引中的名字返回空列表（调用方退回全文查找）。
        assert!(index.lookup("missing").unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存写出后重新读出结果不变；文件改动、删除和新增按修改时间和大小发现，只重新读取这些文件。
    #[test]
    fn cache_reloads_and_reindexes_only_changes() {
        let dir = temp_dir("cache");
        let cache =
            std::env::temp_dir().join(format!("winc-symbol-cache-{}.bin", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&cache);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "class Staff:\n    pass\n").unwrap();
        std::fs::write(dir.join("b.py"), "def helper():\n    pass\n").unwrap();

        let options = || IndexOptions::default();
        let first = SymbolIndex::build_now(&dir, &[], options(), Some(&cache));
        assert_eq!(first.lookup("Staff").unwrap()[0].relative, "a.py");
        assert_eq!(first.lookup("helper").unwrap()[0].relative, "b.py");
        assert!(cache.is_file(), "建好后写出缓存");

        // 没有改动：从缓存读出，结果相同。
        let cached = SymbolIndex::build_now(&dir, &[], options(), Some(&cache));
        assert_eq!(cached.lookup("Staff").unwrap()[0].relative, "a.py");
        assert_eq!(cached.lookup("helper").unwrap()[0].relative, "b.py");

        // 改动 b.py（长度不同，时间戳精度不够时也能发现），删除 a.py，新增 c.py。
        std::fs::write(dir.join("b.py"), "def greet():\n    pass\n").unwrap();
        std::fs::remove_file(dir.join("a.py")).unwrap();
        std::fs::write(dir.join("c.py"), "class Team:\n    pass\n").unwrap();

        let refreshed = SymbolIndex::build_now(&dir, &[], options(), Some(&cache));
        assert!(
            refreshed.lookup("helper").unwrap().is_empty(),
            "改名的旧名字要去掉"
        );
        assert_eq!(refreshed.lookup("greet").unwrap()[0].relative, "b.py");
        assert!(
            refreshed.lookup("Staff").unwrap().is_empty(),
            "删除的文件要去掉"
        );
        assert_eq!(refreshed.lookup("Team").unwrap()[0].relative, "c.py");

        // 选项变了：旧缓存不适用，按新选项重新建立，恢复后再建一次。
        let without_python = IndexOptions {
            exclude: vec!["python".to_owned()],
            ..IndexOptions::default()
        };
        let narrowed = SymbolIndex::build_now(&dir, &[], without_python, Some(&cache));
        assert!(narrowed.lookup("greet").unwrap().is_empty());
        let restored = SymbolIndex::build_now(&dir, &[], options(), Some(&cache));
        assert_eq!(restored.lookup("greet").unwrap()[0].relative, "b.py");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&cache);
    }

    /// 设置中可以排除语言、跳过依赖目录和构建输出目录；关掉对应的跳过选项后收录。
    #[test]
    fn honours_excluded_languages_and_skipped_folders() {
        let dir = temp_dir("options");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/app.py"), "def alpha():\n    pass\n").unwrap();
        std::fs::write(dir.join("src/app.go"), "func beta() {\n}\n").unwrap();
        std::fs::write(dir.join("dist/bundle.js"), "function gamma() {}\n").unwrap();
        std::fs::write(dir.join("node_modules/pkg/lib.js"), "function delta() {}\n").unwrap();

        let defaults = SymbolIndex::build_now(&dir, &[], IndexOptions::default(), None);
        assert!(defaults.lookup("alpha").unwrap()[0].relative == "src/app.py");
        assert!(defaults.lookup("beta").unwrap()[0].relative == "src/app.go");
        assert!(
            defaults.lookup("gamma").unwrap().is_empty(),
            "默认跳过 dist"
        );
        assert!(
            defaults.lookup("delta").unwrap().is_empty(),
            "默认跳过 node_modules"
        );

        let narrow = IndexOptions {
            exclude: vec!["python".to_owned()],
            ..IndexOptions::default()
        };
        let index = SymbolIndex::build_now(&dir, &[], narrow, None);
        assert!(
            index.lookup("alpha").unwrap().is_empty(),
            "被排除的语言不收录"
        );
        assert!(!index.lookup("beta").unwrap().is_empty());

        let open = IndexOptions {
            skip_dependencies: false,
            skip_build: false,
            ..IndexOptions::default()
        };
        let index = SymbolIndex::build_now(&dir, &[], open, None);
        assert!(
            !index.lookup("gamma").unwrap().is_empty(),
            "不跳过时不漏掉 dist"
        );
        assert!(
            !index.lookup("delta").unwrap().is_empty(),
            "不跳过时不漏掉 node_modules"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
