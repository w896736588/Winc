//! 转到定义（`F12`、`⌘` 单击、右键菜单）和查找引用（`⇧F12`）。
//!
//! 不使用语言服务：定义按常见的声明写法在文本中查找（见 `winc_editor::definition`），先查当前文件；
//! 项目中的定义先查工作区符号索引（见 `winc_editor::SymbolIndex`，在第一次需要时于后台建立），
//! 没有收录这个名字时再在后台搜索整个项目；查找引用是全字匹配、区分大小写的全局搜索。光标原本就在
//! 定义上时，转到定义改为列出该名字的用法：局部变量只在当前文件中找，其余在项目中全字搜索后，再在
//! 后台按词法分析剔除注释、字符串、无关语言和其他同名定义中的命中（见 `winc_editor::usages`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use ropey::Rope;
use winc_core::movement;
use winc_editor::definition::{self, Finder, Kind};
use winc_editor::usages::{self, Filter};
use winc_editor::{FileResult, MAX_SEARCH_THREADS, ProjectSearch, Query};
use winc_i18n::t;
use winc_syntax::Language;

use super::*;
use crate::layout::SidebarView;

/// 一处定义的位置。
#[derive(Debug, Clone)]
pub struct Place {
    /// 文件路径。
    pub path: PathBuf,
    /// 相对于已打开目录的路径，在列表中显示。
    pub relative: String,
    /// 行号（从 0 起）。
    pub line: usize,
    /// 名字在该行的列（字符偏移，从 0 起）；为 `None` 时打开后按定义的写法在该行里找。
    pub col: Option<usize>,
    /// 该行的内容（去掉行首空白）。
    pub preview: String,
    /// 名字在 `preview` 中出现的各个范围（字符偏移），浮窗中铺上底色；书签没有。
    ///
    /// 同一行有几处用法时合并成一项，这里记下合并后的范围。
    pub marks: Vec<std::ops::Range<usize>>,
    /// 来自符号索引时，定义所属的类型或模块：接收者类型已知时据此只在同类型中找成员。
    pub owner: Option<String>,
}

/// 卡片中列出的内容种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacesKind {
    /// 多处定义。
    Definitions,
    /// 一个名字的多处用法（在定义上转到定义时）。
    Usages,
    /// 一个文件中的书签。
    Bookmarks,
    /// 在文件中查找的各处匹配。
    Search,
}

impl PlacesKind {
    /// 返回卡片右上角显示的数量文字，`n` 为条目数。
    pub fn count(self, n: usize) -> String {
        match self {
            PlacesKind::Definitions => t!("editor-places-definitions-count", count = n),
            PlacesKind::Usages => t!("editor-places-usages-count", count = n),
            PlacesKind::Bookmarks => t!("editor-places-bookmarks-count", count = n),
            PlacesKind::Search => t!("editor-places-search-count", count = n),
        }
    }
}

/// 在位置浮窗中列出的多处位置（多处定义、用法或书签）。
#[derive(Debug, Clone)]
pub struct Places {
    /// 列出的内容种类。
    pub kind: PlacesKind,
    /// 被查找的名字；书签时为空串。
    pub name: String,
    /// 列出的各处位置。
    pub items: Vec<Place>,
    /// 发起查找的文件相对于已打开目录的路径，显示在标题行。
    pub origin: String,
    /// 浮窗挂在当前文档的哪个位置（字符偏移）：光标所在名字的开头，浮窗出现在这一行的下方
    /// （放不下时在上方），与它左对齐。
    pub anchor: usize,
    /// 全部条目中最宽的「文件名 + 行号」：（界面字号、小字号、宽度），见 [`Places::widest_label`]。
    widest_label: std::cell::Cell<Option<(f32, f32, f32)>>,
}

impl Places {
    /// 返回在文件中查找列出的各处匹配，`query` 是查询的文字。
    pub fn search(query: String, items: Vec<Place>) -> Places {
        Places {
            kind: PlacesKind::Search,
            name: query,
            items,
            origin: String::new(),
            anchor: 0,
            widest_label: Default::default(),
        }
    }

    /// 返回全部条目中最宽的「文件名 + 行号」的宽度（逻辑像素），由 `measure` 量出。
    ///
    /// 这一栏的宽度只取决于条目和字号（`sizes`：界面字号、小字号），条目在浮窗打开期间不变，
    /// 所以量一次记下来，字号变了才重新量；否则浮窗每画一帧（淡入、筛选文字的光标闪烁）都要把
    /// 成千上万个用法量一遍。
    pub fn widest_label(&self, sizes: (f32, f32), measure: impl FnOnce(&[Place]) -> f32) -> f32 {
        match self.widest_label.get() {
            Some((ui, small, width)) if (ui, small) == sizes => width,
            _ => {
                let width = measure(&self.items);
                self.widest_label.set(Some((sizes.0, sizes.1, width)));
                width
            }
        }
    }
}

/// 搜索开始后超过该时长还没结果，就在名字旁提示正在查找，与其他「正在读取」的提示相同。
const SEARCH_HINT_DELAY: Duration = Duration::from_millis(300);

/// 搜索进行中更新进度提示（已经找到的匹配数、正在核对的匹配数）的间隔。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// 正在后台搜索整个项目的定义或用法。
///
/// 分两个阶段：先全字或按定义写法搜索整个项目，再在后台线程中逐个文件做词法分析，剔除不算数的
/// 命中（`filtering`）。
pub struct DefinitionSearch {
    /// 后台的项目搜索。
    engine: ProjectSearch,
    /// 被查找的名字。
    name: String,
    /// 判断命中是否算数（注释、字符串、无关语言、其他同名定义）。
    filter: Arc<Filter>,
    /// 第二阶段：后台线程剔除不算数的命中后送回的结果；为 `None` 时还在第一阶段。
    filtering: Option<mpsc::Receiver<Vec<FileResult>>>,
    /// 发起查找的文件的语言，用于对结果排序。
    language: &'static Language,
    /// 发起查找的文件。
    ///
    /// 该文件已查过，结果中排除；与该文件同一目录的结果排在前面。
    from: PathBuf,
    /// 当前文件中的定义是否只有光标所在的这一处。
    at_caret: bool,
    /// 找的是用法而不是定义时，光标所在的那一行（行号从 0 起）：定义本身在这一行，结果中排除。
    usages_of: Option<usize>,
    /// 成员访问的接收者类型名和当前文件中的同名定义；
    /// 不是成员访问或接收者的类型定义在当前文件中时为 `None`。
    ///
    /// 结果中有以该类型命名的文件时只取这些文件，否则一并列出当前文件中的定义。
    receiver: Option<(String, Option<Place>)>,
    /// 搜索的根目录和条件（由 `start_place_search` 填入）；在项目里找不到定义时用它们再到依赖
    /// 目录里找一遍。
    scope: Option<(PathBuf, Query)>,
    /// 是否已经在依赖目录里找过（见 [`App::search_dependencies`]）。
    in_dependencies: bool,
    /// 下一次更新进度提示的时刻；不再提示时为 `None`。
    next_tick: Option<Instant>,
    /// 是否已经显示过进度提示：搜索结束时收起这条提示。
    hinted: bool,
    /// 第二阶段要核对的匹配数，用于进度提示。
    candidates: usize,
    /// 发起查找时的文档和光标位置（由 `start_place_search` 填入）。
    ///
    /// 搜索结束时用户已经换了标签或挪了光标，就不再弹浮窗、提示或跳转：
    /// 把结果放到其他位置只会打断用户。
    origin: Option<(DocumentId, usize)>,
}

impl App {
    /// 返回光标所在名字的字符范围（有选区时为选区）；光标处没有名字时返回 `None`。
    ///
    /// 名字由当前文件的语言决定（见 [`Language::name_range`]）：CSS 的 `my-class`、PHP 的
    /// `$total`、Ruby 的 `valid?` 都是一个名字。
    pub(in crate::app) fn name_span(&self) -> Option<(usize, usize)> {
        let doc = self.editor.active()?;
        let selection = doc.selection;
        if !selection.is_empty() {
            return Some((selection.from(), selection.to()));
        }
        let text = doc.text();
        let (line, col) = text.pos_to_line_col(selection.head);
        let chars: Vec<char> = text.line_content(line).slice().chars().collect();
        let range = doc.highlighter.language().name_range(&chars, col)?;
        let start = text.line_start(line);
        Some((start + range.start, start + range.end))
    }

    /// 返回光标所在的名字（有选区时为选中的文字）。
    ///
    /// 不是当前语言中的名字时在名字上方提示并返回 `None`。
    fn name_at_caret(&mut self) -> Option<String> {
        let doc = self.editor.active()?;
        if !doc.shows_text() {
            return None;
        }
        let language = doc.highlighter.language();
        let word = self
            .name_span()
            .and_then(|(from, to)| doc.text_range(from, to))
            .unwrap_or_default();
        if language.is_name(&word) {
            Some(word)
        } else {
            self.hint_at_name(t!("editor-definition-no-name"));
            None
        }
    }

    /// 选中光标所在的名字，用在开始搜索之前给出即时反馈；光标处不是名字时不动选区。
    ///
    /// 项目搜索可能要几秒，选中状态让用户立刻看到在查找哪个名字，结束后再跳转或列出候选。
    pub(in crate::app) fn select_name_at_caret(&mut self) {
        let Some((from, to)) = self.name_span() else {
            return;
        };
        let named = self
            .editor
            .active()
            .and_then(|doc| doc.text_range(from, to).map(|word| (doc, word)))
            .is_some_and(|(doc, word)| doc.highlighter.language().is_name(&word));
        if named {
            self.select(from, to);
        }
    }

    /// 转到光标所在名字的定义（`F12`、`⌘` 单击）。
    ///
    /// 当前文件内有定义时，跳到离光标最近的一处：优先取光标之前（含同一行）最近的一处，光标之前没有
    /// 时取第一处；没有定义时先查工作区符号索引，索引没有收录这个名字时在项目中后台搜索，只有一处就
    /// 直接跳转，有多处则列出供选择。光标原本就在定义上时改为列出用法：局部变量只在当前文件中找
    /// （见 [`Self::show_local_usages`]），其余在项目中找（见 [`Self::show_usages`]）；重新赋值
    /// （之前已有同名的定义）时不算在定义上。
    pub(in crate::app) fn go_to_definition(&mut self) {
        let Some(name) = self.name_at_caret() else {
            return;
        };
        let Some(doc) = self.editor.active() else {
            return;
        };
        let language = doc.highlighter.language();
        let ignore_case = Filter::ignores_case(&name, language);
        let (Some(local), Some(filter)) = (
            Finder::new(&name, true, ignore_case),
            Filter::new(&name, language),
        ) else {
            return;
        };
        let caret = doc.selection.head;
        let (caret_line, caret_col) = doc.text().pos_to_line_col(caret);
        let path = doc.path.clone();

        let here = doc
            .text()
            .rope()
            .map(|r| local.in_text(r, language))
            .unwrap_or_default();
        let len = name.chars().count();
        let at_caret =
            |line: usize, col: usize| line == caret_line && (col..=col + len).contains(&caret_col);
        let caret_definition = here.iter().find(|(line, d)| at_caret(*line, d.col));
        let reassigned = caret_definition.is_some_and(|(line, d)| {
            d.kind == Kind::Assignment && here.iter().any(|(earlier, _)| earlier < line)
        });
        if let Some(&(line, d)) = caret_definition
            && !reassigned
        {
            if d.is_local() {
                self.show_local_usages(filter, line);
                return;
            }
            if self.editor.root().is_some() {
                self.show_usages(name, line, filter);
                return;
            }
        }
        let elsewhere: Vec<(usize, usize)> = here
            .iter()
            .filter(|(line, d)| !at_caret(*line, d.col))
            .map(|(line, d)| (*line, d.col))
            .collect();
        let nearest = elsewhere
            .iter()
            .filter(|(line, _)| *line <= caret_line)
            .max_by_key(|(line, _)| *line)
            .or_else(|| elsewhere.first())
            .copied();
        let root = self.editor.root().map(Path::to_path_buf);
        // `service.name(…)`：接收者的类型定义在其他文件中时，
        // 当前文件中的同名方法不是目标，改为在项目中查找。
        let foreign = root.as_ref().and(doc.text().rope()).and_then(|text| {
            let type_name = definition::receiver_type(text, caret_line, caret_col, &name)?;
            let own = path
                .as_deref()
                .and_then(|p| definition::named_after(p, &type_name))
                == Some(true)
                || Finder::new(&type_name, false, false)
                    .is_some_and(|finder| !finder.in_text(text, language).is_empty());
            (!own).then_some((type_name, text))
        });
        let receiver = foreign.map(|(type_name, text)| {
            let file = path.clone().unwrap_or_default();
            let relative = self.relative_name();
            let local = nearest.map(|(at, col)| local_place(&file, &relative, text, at, col, len));
            (type_name, local)
        });
        if let Some((line, col)) = nearest
            && receiver.is_none()
        {
            self.jump_to(line, col, len);
            return;
        }

        let Some(root) = root else {
            return self.no_definition(&name, !here.is_empty());
        };
        // 先查工作区符号索引：命中时不必遍历目录、逐个读取文件。索引在第一次需要时于后台建立，
        // 还没建好或没有收录这个名字（非代码文件、过大的文件）时退回全文查找。名字不区分大小写
        // 的语言（PHP 等）按小写查。
        self.ensure_symbol_index(&root);
        let found = match ignore_case {
            true => self.symbol_index.lookup_ignore_case(&name),
            false => self.symbol_index.lookup(&name),
        };
        if let Some(found) = found
            && !found.is_empty()
        {
            let from = path.clone().unwrap_or_default();
            let dir = from.parent().unwrap_or(Path::new("/")).to_path_buf();
            let mut items = self.symbol_places(found, &name);
            arrange_definitions(&mut items, &from, language, &dir, &receiver, &filter);
            match items.len() {
                1 => {
                    self.open_place(&items[0], &name);
                    return;
                }
                n if n > 1 => {
                    self.show_places(PlacesKind::Definitions, name, items);
                    self.request_redraw();
                    return;
                }
                // 索引中的位置都已失效（文件刚被删改）：退回全文查找。
                _ => {}
            }
        }
        let query = definition::query(&name, ignore_case);
        let search = DefinitionSearch {
            engine: ProjectSearch::default(),
            name,
            filter: Arc::new(filter),
            filtering: None,
            language,
            from: path.unwrap_or_default(),
            at_caret: !here.is_empty(),
            usages_of: None,
            receiver,
            origin: None,
            scope: None,
            in_dependencies: false,
            next_tick: Some(Instant::now() + SEARCH_HINT_DELAY),
            hinted: false,
            candidates: 0,
        };
        self.start_place_search(search, &root, &query);
    }

    /// 列出局部变量 `filter` 的名字在所在函数中的用法；`line` 是定义所在的行，结果中排除。
    ///
    /// 局部变量只在所在的函数中可见：其他文件中、同一文件其他函数中的同名局部变量都与它无关。
    /// 函数按缩进划分的块确定（见 [`usages::function_scope`]），认不出所在函数时在整个文件中找。
    fn show_local_usages(&mut self, filter: Filter, line: usize) {
        let tab_width = self.editor.active_tab_width();
        let Some(blocks) = self.editor.active_mut().map(|d| d.fold_ranges(tab_width)) else {
            return;
        };
        let Some(doc) = self.editor.active() else {
            return;
        };
        let Some(text) = doc.text().rope() else {
            return;
        };
        let language = doc.highlighter.language();
        let scope = usages::function_scope(text, language, &blocks, line);
        let found: Vec<(usize, usize)> = filter
            .occurrences(text)
            .into_iter()
            .filter(|&(at, _)| scope.is_none_or(|(start, end)| (start..=end).contains(&at)))
            .collect();
        let keep = filter.usages(language, text, &found);
        let path = doc.path.clone().unwrap_or_default();
        let relative = self.relative_name();
        let name_len = filter.name_len();
        let mut items: Vec<Place> = found
            .into_iter()
            .zip(keep)
            .filter(|&((at, _), keep)| keep && at != line)
            .map(|((at, col), _)| local_place(&path, &relative, text, at, col, name_len))
            .collect();
        merge_lines(&mut items);
        let name = filter.name();
        match items.len() {
            0 => self.hint_at_name(t!("editor-usages-not-found", name = name.as_str())),
            1 => self.open_place(&items[0], &name),
            _ => self.show_places(PlacesKind::Usages, name, items),
        }
        self.request_redraw();
    }

    /// 返回当前文档与项目搜索结果同样写法的相对路径；没有路径或没有打开目录时返回文档名。
    fn relative_name(&self) -> String {
        let Some(doc) = self.editor.active() else {
            return String::new();
        };
        match (doc.path.as_deref(), self.editor.workspace.as_ref()) {
            (Some(path), Some(workspace)) => workspace.labelled_path(path),
            _ => doc.name().into_owned(),
        }
    }

    /// 确保符号索引已为当前工作区建立；第一次调用时先在后台读取缓存，再校正变化的文件。
    ///
    /// 设置中的「符号索引」一组改变后再次调用，索引按新选项重新建立。
    fn ensure_symbol_index(&mut self, root: &Path) {
        // 没有事件循环（测试）时不建立：结果要等后台，测试中直接走同步的全文查找。
        if self.link.proxy.is_none() {
            return;
        }
        let extra = self.extra_folders();
        let options = self.symbol_options();
        self.symbol_index
            .ensure(root, &extra, options, Self::symbol_cache_dir());
    }

    /// 为当前工作区在后台建立（或从缓存读出并校正）符号索引；打开目录时调用。
    ///
    /// 进度显示在状态栏右侧，见 [`crate::view::status`]。
    pub(in crate::app) fn index_symbols(&mut self) {
        if self.link.proxy.is_none() {
            return;
        }
        let Some(root) = self.editor.root().map(Path::to_path_buf) else {
            return;
        };
        self.ensure_symbol_index(&root);
    }

    /// 按设置中的「符号索引」一组重新建立当前工作区的索引；没有打开目录时不处理。
    pub(in crate::app) fn refresh_symbol_index(&mut self) {
        self.index_symbols();
    }

    /// 设置中「符号索引」一组对应的索引选项。
    fn symbol_options(&self) -> winc_editor::IndexOptions {
        winc_editor::IndexOptions {
            exclude: self.config.index_exclude.clone(),
            skip_dependencies: self.config.index_ignore_dependencies != Some(false),
            skip_build: self.config.index_ignore_build != Some(false),
            ignore_folders: self.config.index_ignore_folders.clone(),
            ignore_files: self.config.index_ignore_files.clone(),
        }
    }

    /// 缓存符号索引的目录：状态文件所在目录下的 `symbol-index`；确定不了时不用缓存。
    fn symbol_cache_dir() -> Option<PathBuf> {
        let state = winc_config::state_path()?;
        Some(state.parent()?.join("symbol-index"))
    }

    /// 把符号索引中的位置整理成浮窗条目：读入各行的内容作为预览。
    ///
    /// 索引不保存行内容；列出的结果通常只有几处，这几个文件读一遍的开销可以忽略。已经打开的
    /// 文件按内存中的内容取（可能有未保存的修改）。
    fn symbol_places(&self, found: Vec<winc_editor::SymbolPlace>, name: &str) -> Vec<Place> {
        let mut texts: HashMap<PathBuf, Option<Rope>> = HashMap::new();
        let len = name.chars().count();
        let mut items = Vec::with_capacity(found.len());
        for place in found {
            let text = texts
                .entry(place.path.clone())
                .or_insert_with(|| self.file_rope(&place.path));
            let Some(text) = text else {
                continue;
            };
            let line = place.line.min(text.line_count().saturating_sub(1));
            let content: String = text
                .line(line)
                .chars()
                .filter(|&c| c != '\n' && c != '\r')
                .collect();
            let indent = content.chars().take_while(|c| c.is_whitespace()).count();
            let preview: String = content.chars().skip(indent).collect();
            // 名字应当在索引记下的列上；文件改过、列对不上时只按行内容找，打开后再在行内定位。
            let start = place
                .col
                .checked_sub(indent)
                .filter(|&start| preview.chars().skip(start).take(len).eq(name.chars()));
            let marks = match start {
                Some(start) => std::iter::once(start..start + len).collect(),
                None => preview
                    .find(name)
                    .map(|at| preview[..at].chars().count())
                    .map(|start| std::iter::once(start..start + len).collect())
                    .unwrap_or_default(),
            };
            items.push(Place {
                path: place.path,
                relative: place.relative,
                line,
                // 名字不在索引记下的列上（文件刚被改过）时，打开后按定义的写法在该行里重新找。
                col: start.map(|_| place.col),
                preview,
                marks,
                owner: place.owner,
            });
        }
        items
    }

    /// 返回文件的内容：已经打开的文档按内存中的内容，其余从磁盘读取并识别编码。
    fn file_rope(&self, path: &Path) -> Option<Rope> {
        if let Some(doc) = self
            .editor
            .documents
            .iter()
            .find(|doc| doc.path.as_deref() == Some(path))
        {
            return doc.text().rope().cloned();
        }
        let bytes = winc_backend::fs::read(path).ok()?;
        Some(Rope::from_str(
            &winc_editor::encoding::decode_unknown(&bytes).0,
        ))
    }

    /// 在项目中后台查找光标所在的名字 `name` 的用法（全字匹配、区分大小写），`line` 是光标所在
    /// 的行，即定义所在的行，结果中排除。
    ///
    /// 只有一处用法就直接跳转，有多处则列在卡片中（显示文件、行号和那一行的内容），没有则在
    /// 状态栏提示。
    fn show_usages(&mut self, name: String, line: usize, filter: Filter) {
        let (Some(root), Some(doc)) = (
            self.editor.root().map(Path::to_path_buf),
            self.editor.active(),
        ) else {
            return;
        };
        let query = Query {
            text: name.clone(),
            case_sensitive: !filter.ignore_case(),
            whole_word: true,
            ..Query::default()
        };
        let search = DefinitionSearch {
            engine: ProjectSearch::default(),
            name,
            filter: Arc::new(filter),
            filtering: None,
            language: doc.highlighter.language(),
            from: doc.path.clone().unwrap_or_default(),
            at_caret: true,
            usages_of: Some(line),
            receiver: None,
            origin: None,
            scope: None,
            in_dependencies: false,
            next_tick: Some(Instant::now() + SEARCH_HINT_DELAY),
            hinted: false,
            candidates: 0,
        };
        self.start_place_search(search, &root, &query);
    }

    /// 在 `root` 和附加目录下后台开始搜索 `query`，搜索进展由 [`Self::poll_definitions`] 处理。
    ///
    /// 未保存的文档按内存中的内容搜索。
    fn start_place_search(&mut self, mut search: DefinitionSearch, root: &Path, query: &Query) {
        search.engine.set_extra_folders(self.extra_folders());
        let dirty = self.dirty_ropes();
        let proxy = self.link.proxy.clone();
        let wake = move || {
            if let Some(proxy) = &proxy {
                let _ = proxy.send_event(UserEvent::Definitions);
            }
        };
        match search.in_dependencies {
            true => search
                .engine
                .start_in_dependencies(root, query, dirty, wake),
            false => search.engine.start(root, query, dirty, wake),
        }
        search.scope = Some((root.to_path_buf(), query.clone()));
        search.origin = self.editor.active().map(|d| (d.id, d.selection.head));
        self.jumps.definitions = Some(search);
        // 测试中没有事件循环，同步等待搜索结束。
        if self.link.proxy.is_none()
            && let Some(search) = &mut self.jumps.definitions
        {
            search.engine.wait();
        }
        self.poll_definitions();
    }

    /// 处理后台项目搜索（定义或用法）的进展。
    ///
    /// 搜索结束后在后台线程中剔除不算数的命中（测试中没有事件循环，同步进行），全部完成后：只有
    /// 一处结果就跳转，有多处则列出供选择，没有则在状态栏提示。
    pub(in crate::app) fn poll_definitions(&mut self) {
        let Some(mut search) = self.jumps.definitions.take() else {
            return;
        };
        if let Some(receiver) = &search.filtering {
            match receiver.try_recv() {
                Ok(results) => search.engine.results = results,
                Err(mpsc::TryRecvError::Empty) => {
                    self.jumps.definitions = Some(search);
                    return;
                }
                // 后台线程异常退出：结果已交给它，按没有结果处理。
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        } else {
            search.engine.poll();
            if search.engine.is_running() {
                self.jumps.definitions = Some(search);
                return;
            }
            let results = std::mem::take(&mut search.engine.results);
            let dirty: HashMap<PathBuf, Rope> = self.dirty_ropes().into_iter().collect();
            let filter = Arc::clone(&search.filter);
            let usages = search.usages_of.is_some();
            // 没有命中时无需再逐个文件读取、做词法分析。
            if results.is_empty() {
                self.finish_place_search(search);
                return;
            }
            search.candidates = results.iter().map(|file| file.matches.len()).sum();
            match self.link.proxy.clone() {
                None => search.engine.results = filter_results(&filter, usages, results, &dirty),
                Some(proxy) => {
                    let (sender, receiver) = mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = sender.send(filter_results(&filter, usages, results, &dirty));
                        let _ = proxy.send_event(UserEvent::Definitions);
                    });
                    search.filtering = Some(receiver);
                    self.jumps.definitions = Some(search);
                    return;
                }
            }
        }
        self.finish_place_search(search);
    }

    /// 列出搜索并剔除后的结果，或跳转、提示，见 [`Self::poll_definitions`]。
    fn finish_place_search(&mut self, search: DefinitionSearch) {
        let here = self.editor.active().map(|d| (d.id, d.selection.head));
        if here != search.origin {
            // 搜索期间用户换了标签或挪了光标，结果已经用不上；一并收起仍在显示的进度提示。
            if search.hinted {
                self.jumps.name_hint = None;
            }
            return;
        }

        let dir = search.from.parent().unwrap_or(Path::new("/")).to_path_buf();
        let name = search.name.clone();
        let from = &search.from;
        let usages = search.usages_of;
        let mut items: Vec<Place> = search
            .engine
            .results
            .iter()
            // 找定义时当前文件已经查过；找用法时当前文件也要，只排除定义本身所在的那一行。
            .filter(|file| usages.is_some() || file.path != *from)
            .flat_map(|file| {
                file.matches
                    .iter()
                    .filter(move |m| !(file.path == *from && Some(m.line) == usages))
                    .map(|m| Place {
                        path: file.path.clone(),
                        relative: file.relative.clone(),
                        line: m.line,
                        col: usages.map(|_| m.cols.start),
                        preview: m.preview.clone(),
                        marks: name_in_preview(&m.preview, m.highlight.clone(), &name)
                            .into_iter()
                            .collect(),
                        owner: None,
                    })
            })
            .collect();
        if usages.is_some() {
            // 用法：当前文件的排在最前，其余按路径和行号；同一行的几处合并成一项（跳过去时
            // 选中第一处）。
            items.sort_by(|a, b| {
                (a.path != *from, &a.relative, a.line, a.col).cmp(&(
                    b.path != *from,
                    &b.relative,
                    b.line,
                    b.col,
                ))
            });
            merge_lines(&mut items);
        } else {
            arrange_definitions(
                &mut items,
                from,
                search.language,
                &dir,
                &search.receiver,
                &search.filter,
            );
        }

        // 正在查找的提示到此为止。
        if search.hinted {
            self.jumps.name_hint = None;
        }
        match (items.len(), usages) {
            (0, Some(_)) => self.hint_at_name(t!("editor-usages-not-found", name = name.as_str())),
            (0, None) if !search.in_dependencies && !search.at_caret => {
                return self.search_dependencies(search);
            }
            (0, None) => self.no_definition(&name, search.at_caret),
            (1, _) => self.open_place(&items[0], &name),
            (_, Some(_)) => self.show_places(PlacesKind::Usages, name, items),
            (_, None) => self.show_places(PlacesKind::Definitions, name, items),
        }
        self.request_redraw();
    }

    /// 项目里没有找到定义：到依赖目录（`node_modules`、`.venv` 等，通常被 `.gitignore` 忽略）
    /// 里再找一遍；找了 [`SEARCH_HINT_DELAY`] 还没找完时在名字旁提示正在查找。
    fn search_dependencies(&mut self, mut search: DefinitionSearch) {
        let Some((root, query)) = search.scope.take() else {
            return self.no_definition(&search.name, search.at_caret);
        };
        search.next_tick = Some(Instant::now() + SEARCH_HINT_DELAY);
        search.hinted = false;
        search.in_dependencies = true;
        search.filtering = None;
        search.candidates = 0;
        search.engine.results.clear();
        self.start_place_search(search, &root, &query);
    }

    /// 更新名字旁的搜索进度：显示阶段（查找、核对、依赖目录）和已经找到的匹配数；
    /// 返回下一次更新的时刻。
    ///
    /// 第一次在 [`SEARCH_HINT_DELAY`] 之后显示，此后每隔 [`PROGRESS_INTERVAL`] 刷新，
    /// 直到搜索结束或用户按键、点击收起了气泡。搜索期间换到别的标签后不再提示。
    pub(in crate::app) fn definition_tick(&mut self, now: Instant) -> Option<Instant> {
        let (first, text, next) = {
            let search = self.jumps.definitions.as_mut()?;
            let due = search.next_tick?;
            if now < due {
                return Some(due);
            }
            // 结果只对发起查找的那个标签有意义，换到别的标签后不再提示。
            if self.editor.active().map(|doc| doc.id) != search.origin.map(|(id, _)| id) {
                return None;
            }
            let first = !search.hinted;
            search.hinted = true;
            let next = now + PROGRESS_INTERVAL;
            search.next_tick = Some(next);
            (first, progress_text(search), next)
        };
        let shown = self
            .jumps
            .name_hint
            .as_ref()
            .is_some_and(|hint| hint.progress && hint.fade_out.is_none());
        if first || shown {
            self.progress_hint_at_name(text);
            Some(next)
        } else {
            // 用户按键、点击后气泡已经收起：不再弹出，也不必继续按间隔更新。
            if let Some(search) = &mut self.jumps.definitions {
                search.next_tick = None;
            }
            None
        }
    }

    /// 在状态栏提示没有找到 `name` 的定义。
    ///
    /// `at_caret` 为 `true` 时改为提示光标处本身就是定义。
    fn no_definition(&mut self, name: &str, at_caret: bool) {
        self.hint_at_name(if at_caret {
            t!("editor-definition-at-caret", name = name)
        } else {
            t!("editor-definition-not-found", name = name)
        });
    }

    /// 打开 `place` 所在的文件，并选中其中的名字 `name`。
    pub(super) fn open_place(&mut self, place: &Place, name: &str) {
        let (line, col, name) = (place.line, place.col, name.to_owned());
        self.open_path_then(&place.path, move |app| {
            let col = col.or_else(|| {
                let doc = app.editor.active()?;
                let ignore_case = Filter::ignores_case(&name, doc.highlighter.language());
                let text = doc.text();
                let line = text.line_content(line.min(text.line_count().saturating_sub(1)));
                Finder::new(&name, false, ignore_case)?.in_line(&line.slice().to_string())
            });
            app.jump_to(line, col.unwrap_or(0), name.chars().count());
        });
    }

    /// 选中第 `line` 行第 `col` 列起的 `len` 个字符，并滚动到可见。
    ///
    /// 行号和列号都从 0 起。当前标签刚打开、还在读时，焦点马上给编辑区，读好再跳
    /// （[`App::when_loaded`]）。
    pub(in crate::app) fn jump_to(&mut self, line: usize, col: usize, len: usize) {
        if self.editor.active().is_none() {
            return;
        }
        self.focus = Focus::Editor;
        self.when_loaded(move |app| app.go_to(line, col, len));
    }

    /// 在挂在光标所在名字下方的浮窗中列出 `items`，直接打字可以筛选。
    pub(super) fn show_places(&mut self, kind: PlacesKind, name: String, items: Vec<Place>) {
        let Some(doc) = self.editor.active() else {
            return;
        };
        let anchor = self.name_span().map_or_else(
            || movement::word_at(doc.text(), doc.selection.head).0,
            |s| s.0,
        );
        let origin = self.relative_name();
        self.finish_name_input();
        self.overlays.picker.places_out = None;
        self.jumps.name_hint = None;
        self.overlays.picker.places_wheel = 0.0;
        self.overlays.picker.places_in = self.clock(POPUP_IN);
        self.overlays.picker.branch_rows = None;
        self.overlays.picker.location_rows = None;
        self.overlays.picker.folders = false;
        self.overlays.picker.languages = false;
        self.overlays.picker.vim_list = None;
        self.overlays.picker.remote = None;
        self.overlays.picker.places = Some(Places {
            kind,
            name,
            items,
            origin,
            anchor,
            widest_label: Default::default(),
        });
        self.overlays.picker.open = true;
        self.overlays.picker.doc = self.editor.active().map(|d| d.id);
        self.overlays.picker.input.set_text("");
        self.focus = Focus::Picker;
        self.picker_refresh();
        self.relayout();
        self.reset_blink();
    }

    /// 按输入框的文字筛选列表：路径或所在行的内容包含输入的文字（不区分大小写）即保留。
    pub(in crate::app) fn places_refresh(&mut self) {
        let query = self.overlays.picker.input.text().trim().to_lowercase();
        let Some(places) = &self.overlays.picker.places else {
            return;
        };
        self.overlays.picker.hits = places
            .items
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                query.is_empty()
                    || p.relative.to_lowercase().contains(&query)
                    || p.preview.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect();
    }

    /// 处理列表中的回车：关闭面板并跳转到所选的一处。
    pub(in crate::app) fn places_accept(&mut self) {
        let Some(&i) = self.overlays.picker.hits.get(self.overlays.picker.selected) else {
            return;
        };
        // 所选的一项不存在时面板保持打开，列表也留在原处。
        let Some(mut places) = self.overlays.picker.places.take_if(|p| i < p.items.len()) else {
            return;
        };
        let place = places.items.swap_remove(i);
        // 列表已经取走，关闭面板时不保留淡出的影子：选中一项后就跳转过去，视图已经换了位置，
        // 影子没有意义。
        self.close_picker();
        if places.kind == PlacesKind::Bookmarks {
            // 书签只跳转到所在行，不选中文字。
            self.jump_to(place.line, 0, 0);
        } else {
            self.open_place(&place, &places.name);
        }
    }

    /// 查找引用（`⇧F12`）：对光标所在的名字做全字匹配、区分大小写的全局搜索。
    ///
    /// 结果显示在侧栏的搜索面板中。没有打开目录时在状态栏提示。
    pub(in crate::app) fn find_references(&mut self) {
        if self.editor.workspace.is_none() {
            self.flash(t!("editor-open-folder-first"));
            return;
        }
        let Some(name) = self.name_at_caret() else {
            return;
        };

        self.panels.global.case_sensitive = true;
        self.panels.global.whole_word = true;
        self.panels.global.regex = false;
        self.panels.global.query.remember();
        self.panels.global.query.set_text(&name);
        self.set_sidebar_view(SidebarView::Search);
        if !self.panels.sidebar.visible {
            self.set_sidebar(true);
        }
        self.global_changed(true);
        self.focus = Focus::Results;
        self.relayout();
        self.request_redraw();
    }
}

/// 整理找到的定义：有相关语言中的定义时只留这些；有接收者类型时优先以该类型命名的文件，没有
/// 时补上当前文件中的同名定义（类型推断可能有误）；按相关语言、同一目录和文件名排序。
fn arrange_definitions(
    items: &mut Vec<Place>,
    from: &Path,
    language: &'static Language,
    dir: &Path,
    receiver: &Option<(String, Option<Place>)>,
    filter: &Filter,
) {
    // 其他语言中的同名定义（文档、数据文件）通常无关。
    if items.iter().any(|place| filter.is_related(&place.path)) {
        items.retain(|place| filter.is_related(&place.path));
    }
    let exact = |place: &Place, type_name: &str| definition::named_after(&place.path, type_name);
    // 索引里记下了定义所属的类型：接收者类型已知时先按它找，同名方法分散在各文件（Go、PHP 的
    // 大型项目）也能只留下目标类型中的几处；不依赖文件是否以类型命名。
    let owned = |place: &Place, type_name: &str| {
        place
            .owner
            .as_deref()
            .is_some_and(|owner| owner.eq_ignore_ascii_case(type_name))
    };
    let type_name = receiver.as_ref().map(|(name, _)| name.as_str());
    if let Some(name) = type_name {
        if items.iter().any(|place| owned(place, name)) {
            items.retain(|place| owned(place, name));
        } else if items.iter().any(|place| exact(place, name).is_some()) {
            items.retain(|place| exact(place, name).is_some());
        } else if let Some((_, Some(local))) = receiver {
            items.push(local.clone());
        }
    }
    items.sort_by(|a, b| {
        let key = |place: &Place| {
            let unowned = !type_name.is_some_and(|name| owned(place, name));
            let named = type_name.and_then(|name| exact(place, name)) != Some(true);
            (
                unowned,
                named,
                place.path != *from,
                definition::rank(&place.path, language, dir),
            )
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.relative.cmp(&b.relative))
    });
}

/// 返回搜索进度提示的文字：阶段（查找、核对、依赖目录）和已经找到的匹配数。
///
/// 还没有命中时只写正在查找；第二阶段（逐个文件核对）显示要核对的匹配数。
fn progress_text(search: &DefinitionSearch) -> String {
    let name = search.name.as_str();
    let count = match search.filtering.is_some() {
        true => search.candidates,
        false => search.engine.total(),
    };
    match count {
        0 if search.usages_of.is_some() => t!("editor-usages-searching", name = name),
        0 if search.in_dependencies => {
            t!("editor-definition-in-dependencies", name = name)
        }
        0 => t!("editor-definition-searching", name = name),
        _ if search.filtering.is_some() && search.usages_of.is_some() => {
            t!("editor-usages-filtering", name = name, count = count)
        }
        _ if search.filtering.is_some() => {
            t!("editor-definition-filtering", name = name, count = count)
        }
        _ if search.in_dependencies => {
            t!(
                "editor-definition-in-dependencies-count",
                name = name,
                count = count
            )
        }
        _ if search.usages_of.is_some() => {
            t!("editor-usages-searching-count", name = name, count = count)
        }
        _ => t!(
            "editor-definition-searching-count",
            name = name,
            count = count
        ),
    }
}

/// 逐个文件剔除不算数的命中（见 [`Filter::usages`]、[`Filter::definitions`]），返回剩下的结果；
/// `usages` 为真时找的是用法，否则是定义。
///
/// 每个文件都要读入并做词法分析，常见的名字可能命中数千个文件，因此多个线程并行处理；线程数上限
/// 与项目搜索相同。结果的顺序不变。
fn filter_results(
    filter: &Filter,
    usages: bool,
    mut results: Vec<FileResult>,
    dirty: &HashMap<PathBuf, Rope>,
) -> Vec<FileResult> {
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .min(MAX_SEARCH_THREADS);
    let files = Mutex::new(results.iter_mut());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                // 先取出下一个文件、放开锁，再处理：写在 `while let` 的条件里时，锁要到循环体
                // 结束才放开，各线程只能轮流工作。
                loop {
                    let next = files.lock().unwrap_or_else(PoisonError::into_inner).next();
                    let Some(file) = next else {
                        break;
                    };
                    filter_file(filter, usages, file, dirty);
                }
            });
        }
    });
    results.retain(|file| !file.matches.is_empty());
    results
}

/// 剔除一个文件中不算数的命中。
///
/// 有未保存修改的文件按 `dirty` 中的内容判断，其余从磁盘读取并识别编码，与项目搜索相同；读不到
/// 的文件保留全部命中。
fn filter_file(
    filter: &Filter,
    usages: bool,
    file: &mut FileResult,
    dirty: &HashMap<PathBuf, Rope>,
) {
    let text = match dirty.get(&file.path) {
        Some(rope) => rope.clone(),
        None => match winc_backend::fs::read(&file.path) {
            Ok(bytes) => Rope::from_str(&winc_editor::encoding::decode_unknown(&bytes).0),
            Err(_) => return,
        },
    };
    let language = usages::language_of(&file.path, &text);
    let positions: Vec<(usize, usize)> = file
        .matches
        .iter()
        .map(|m| (m.line, m.cols.start))
        .collect();
    let keep = if usages {
        filter.usages(language, &text, &positions)
    } else {
        filter.definitions(language, &text, &positions)
    };
    let mut keep = keep.into_iter();
    file.matches.retain(|_| keep.next().unwrap_or(true));
}

/// 把同一文件同一行的几处用法合并成一项，名字的范围合在一起；`items` 已按文件和行号排序。
fn merge_lines(items: &mut Vec<Place>) {
    items.dedup_by(|next, kept| {
        let same = next.path == kept.path && next.line == kept.line;
        if same {
            kept.marks.append(&mut next.marks);
        }
        same
    });
}

/// 返回当前文件中第 `line` 行第 `col` 列（均从 0 起）起、长 `len` 个字符的名字所在的位置，
/// 预览是去掉行首空白的那一行。
fn local_place(
    path: &Path,
    relative: &str,
    text: &Rope,
    line: usize,
    col: usize,
    len: usize,
) -> Place {
    let content: String = text
        .line(line)
        .chars()
        .filter(|&c| c != '\n' && c != '\r')
        .collect();
    let indent = content.chars().take_while(|c| c.is_whitespace()).count();
    let preview: String = content.chars().skip(indent).collect();
    let start = col.saturating_sub(indent);
    let mark = start..start + len;
    Place {
        path: path.to_path_buf(),
        relative: relative.to_owned(),
        line,
        col: Some(col),
        preview,
        marks: vec![mark],
        owner: None,
    }
}

/// 返回名字 `name` 在预览 `preview` 中的字符范围；`highlight` 是搜索命中的字节范围。
///
/// 找用法时命中的就是名字本身；找定义时命中的是整个声明（`def load`、`load = (a) =>`），取其中
/// 最后一次出现的名字（`func (r T) name` 中是方法名，不是接收者）。命中被截在预览之外时
/// 返回 `None`。
fn name_in_preview(
    preview: &str,
    highlight: std::ops::Range<usize>,
    name: &str,
) -> Option<std::ops::Range<usize>> {
    let hit = preview.get(highlight.clone())?;
    let start = highlight.start + hit.rfind(name)?;
    let from = preview[..start].chars().count();
    Some(from..from + name.chars().count())
}
