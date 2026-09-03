//! Internal B-tree rope backing [`crate::TextBuffer`].
//!
//! Leaves hold short UTF-8 chunks; internal nodes hold up to [`MAX_CHILDREN`]
//! children and a cached [`TextInfo`] summary (bytes, chars, line breaks) for
//! their whole subtree. Every position query descends the tree using those
//! summaries, so byte/char/line conversions cost O(log n) instead of scanning
//! the document, and an edit rewrites one chunk plus the nodes on its path
//! rather than the whole file.
//!
//! Nodes sit behind `Arc` and are edited copy-on-write. A clone of the root is
//! therefore O(1), which is what makes an immutable snapshot cheap enough to
//! hand to the renderer on every frame.
//!
//! This module is deliberately private: correctness here is enforced by
//! [`Node::validate`] and by the public tests on `TextBuffer`.

use std::sync::Arc;

/// Maximum bytes in one leaf chunk. Small enough that a chunk rewrite is a
/// memcpy of a page or so, large enough that the tree stays shallow.
pub(crate) const MAX_LEAF_BYTES: usize = 1024;

/// Fill factor used when building a rope from existing text: leaves start with
/// room to grow so that typing does not immediately split every chunk.
pub(crate) const TARGET_LEAF_BYTES: usize = MAX_LEAF_BYTES * 3 / 4;

/// Maximum children of an internal node.
pub(crate) const MAX_CHILDREN: usize = 8;

/// Summary of a subtree, cached on every node.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextInfo {
    pub bytes: usize,
    pub chars: usize,
    pub line_breaks: usize,
}

impl TextInfo {
    pub(crate) fn from_str(text: &str) -> Self {
        TextInfo {
            bytes: text.len(),
            chars: count_chars(text),
            line_breaks: count_line_breaks(text),
        }
    }

    fn combine(self, other: Self) -> Self {
        TextInfo {
            bytes: self.bytes + other.bytes,
            chars: self.chars + other.chars,
            line_breaks: self.line_breaks + other.line_breaks,
        }
    }

    /// True when the chunk is pure ASCII, so byte and char offsets coincide.
    fn is_ascii(&self) -> bool {
        self.bytes == self.chars
    }
}

#[inline]
pub(crate) fn count_chars(text: &str) -> usize {
    // `chars().count()` is a simple byte scan for the leading-byte pattern and
    // is fast enough at chunk size.
    text.chars().count()
}

#[inline]
fn count_line_breaks(text: &str) -> usize {
    // Only `\n` is counted: the buffer stores text with normalized line
    // endings, and a lone `\r` inside a chunk is ordinary text.
    text.as_bytes().iter().filter(|&&b| b == b'\n').count()
}

#[derive(Clone, Debug)]
pub(crate) enum Node {
    Leaf { text: String, info: TextInfo },
    Internal { children: Vec<Arc<Node>>, info: TextInfo },
}

impl Node {
    pub(crate) fn empty() -> Node {
        Node::Leaf { text: String::new(), info: TextInfo::default() }
    }

    pub(crate) fn leaf(text: String) -> Node {
        let info = TextInfo::from_str(&text);
        Node::Leaf { text, info }
    }

    pub(crate) fn internal_pair(left: Arc<Node>, right: Arc<Node>) -> Node {
        Node::internal(vec![left, right])
    }

    fn internal(children: Vec<Arc<Node>>) -> Node {
        let info = sum_info(&children);
        Node::Internal { children, info }
    }

    #[inline]
    pub(crate) fn info(&self) -> TextInfo {
        match self {
            Node::Leaf { info, .. } => *info,
            Node::Internal { info, .. } => *info,
        }
    }

    #[inline]
    pub(crate) fn len_chars(&self) -> usize {
        self.info().chars
    }

    pub(crate) fn depth(&self) -> usize {
        match self {
            Node::Leaf { .. } => 0,
            Node::Internal { children, .. } => 1 + children.first().map(|c| c.depth()).unwrap_or(0),
        }
    }

    /// Character offset at which line `line` starts. `line == 0` is offset 0;
    /// a line past the end returns the document length.
    pub(crate) fn line_to_char(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        match self {
            Node::Leaf { text, info } => char_after_line_break(text, info, line),
            Node::Internal { children, info } => {
                if line > info.line_breaks {
                    return info.chars;
                }
                let mut chars_before = 0;
                let mut breaks_before = 0;
                for child in children {
                    let child_info = child.info();
                    if breaks_before + child_info.line_breaks >= line {
                        return chars_before + child.line_to_char(line - breaks_before);
                    }
                    breaks_before += child_info.line_breaks;
                    chars_before += child_info.chars;
                }
                chars_before
            }
        }
    }

    /// Line containing `char_idx`.
    pub(crate) fn char_to_line(&self, char_idx: usize) -> usize {
        match self {
            Node::Leaf { text, info } => line_breaks_before_char(text, info, char_idx),
            Node::Internal { children, .. } => {
                let mut remaining = char_idx;
                let mut lines = 0;
                for child in children {
                    let child_info = child.info();
                    if remaining <= child_info.chars {
                        return lines + child.char_to_line(remaining);
                    }
                    remaining -= child_info.chars;
                    lines += child_info.line_breaks;
                }
                lines
            }
        }
    }

    pub(crate) fn char_to_byte(&self, char_idx: usize) -> usize {
        match self {
            Node::Leaf { text, info } => byte_of_char(text, info, char_idx),
            Node::Internal { children, .. } => {
                let mut remaining = char_idx;
                let mut bytes = 0;
                for child in children {
                    let child_info = child.info();
                    if remaining <= child_info.chars {
                        return bytes + child.char_to_byte(remaining);
                    }
                    remaining -= child_info.chars;
                    bytes += child_info.bytes;
                }
                bytes
            }
        }
    }

    pub(crate) fn byte_to_char(&self, byte_idx: usize) -> usize {
        match self {
            Node::Leaf { text, info } => {
                if info.is_ascii() {
                    return byte_idx.min(info.chars);
                }
                let clamped = byte_idx.min(text.len());
                text[..clamped].chars().count()
            }
            Node::Internal { children, .. } => {
                let mut remaining = byte_idx;
                let mut chars = 0;
                for child in children {
                    let child_info = child.info();
                    if remaining <= child_info.bytes {
                        return chars + child.byte_to_char(remaining);
                    }
                    remaining -= child_info.bytes;
                    chars += child_info.chars;
                }
                chars
            }
        }
    }

    pub(crate) fn char_at(&self, char_idx: usize) -> Option<char> {
        match self {
            Node::Leaf { text, info } => {
                let byte = byte_of_char(text, info, char_idx);
                text[byte..].chars().next()
            }
            Node::Internal { children, .. } => {
                let mut remaining = char_idx;
                for child in children {
                    let chars = child.len_chars();
                    // A position exactly at a child boundary belongs to the next
                    // child, because it addresses the character *after* it.
                    if remaining < chars {
                        return child.char_at(remaining);
                    }
                    remaining -= chars;
                }
                None
            }
        }
    }

    /// Appends `[start, end)` (character offsets) to `out`.
    pub(crate) fn append_slice(&self, start: usize, end: usize, out: &mut String) {
        if start >= end {
            return;
        }
        match self {
            Node::Leaf { text, info } => {
                let from = byte_of_char(text, info, start);
                let to = byte_of_char(text, info, end);
                out.push_str(&text[from..to]);
            }
            Node::Internal { children, .. } => {
                let mut offset = 0;
                for child in children {
                    let chars = child.len_chars();
                    let child_start = offset;
                    let child_end = offset + chars;
                    if child_end <= start {
                        offset = child_end;
                        continue;
                    }
                    if child_start >= end {
                        break;
                    }
                    child.append_slice(
                        start.saturating_sub(child_start),
                        (end - child_start).min(chars),
                        out,
                    );
                    offset = child_end;
                }
            }
        }
    }

    /// Structural invariants. Test-only: this walks the whole tree.
    pub(crate) fn validate(&self, expected_depth: usize) -> Result<TextInfo, String> {
        match self {
            Node::Leaf { text, info } => {
                if expected_depth != 0 {
                    return Err(format!("leaf found at depth {expected_depth}, expected 0"));
                }
                if text.len() > MAX_LEAF_BYTES {
                    return Err(format!("leaf of {} bytes exceeds maximum", text.len()));
                }
                let recomputed = TextInfo::from_str(text);
                if recomputed != *info {
                    return Err(format!("leaf info {info:?} does not match text {recomputed:?}"));
                }
                Ok(recomputed)
            }
            Node::Internal { children, info } => {
                if expected_depth == 0 {
                    return Err("internal node found where a leaf was expected".to_string());
                }
                if children.is_empty() {
                    return Err("internal node has no children".to_string());
                }
                if children.len() > MAX_CHILDREN {
                    return Err(format!("internal node has {} children", children.len()));
                }
                let mut total = TextInfo::default();
                for child in children {
                    total = total.combine(child.validate(expected_depth - 1)?);
                }
                if total != *info {
                    return Err(format!(
                        "internal info {info:?} does not match children {total:?}"
                    ));
                }
                Ok(total)
            }
        }
    }
}

fn sum_info(children: &[Arc<Node>]) -> TextInfo {
    children.iter().fold(TextInfo::default(), |acc, child| acc.combine(child.info()))
}

/// Byte offset of character `char_idx` inside one chunk.
#[inline]
fn byte_of_char(text: &str, info: &TextInfo, char_idx: usize) -> usize {
    if info.is_ascii() {
        return char_idx.min(text.len());
    }
    match text.char_indices().nth(char_idx) {
        Some((byte, _)) => byte,
        None => text.len(),
    }
}

/// Number of line breaks before `char_idx` inside one chunk.
fn line_breaks_before_char(text: &str, info: &TextInfo, char_idx: usize) -> usize {
    if info.line_breaks == 0 {
        return 0;
    }
    let byte = byte_of_char(text, info, char_idx);
    count_line_breaks(&text[..byte])
}

/// Character offset just after the `line`th line break inside one chunk.
fn char_after_line_break(text: &str, info: &TextInfo, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut seen = 0;
    for (byte, ch) in text.char_indices() {
        if ch == '\n' {
            seen += 1;
            if seen == line {
                let after = byte + 1;
                return if info.is_ascii() { after } else { text[..after].chars().count() };
            }
        }
    }
    info.chars
}

/// Builds a balanced rope from existing text in one pass.
pub(crate) fn build(text: &str) -> Arc<Node> {
    if text.len() <= MAX_LEAF_BYTES {
        return Arc::new(Node::leaf(text.to_string()));
    }

    let mut level: Vec<Arc<Node>> = Vec::with_capacity(text.len() / TARGET_LEAF_BYTES + 1);
    let mut start = 0;
    while start < text.len() {
        let end = chunk_end(text, start, TARGET_LEAF_BYTES);
        level.push(Arc::new(Node::leaf(text[start..end].to_string())));
        start = end;
    }

    while level.len() > 1 {
        let mut parents = Vec::with_capacity(level.len() / MAX_CHILDREN + 1);
        for group in level.chunks(MAX_CHILDREN) {
            parents.push(Arc::new(Node::internal(group.to_vec())));
        }
        level = parents;
    }
    level.pop().unwrap_or_else(|| Arc::new(Node::empty()))
}

/// End of the chunk starting at `start`, respecting char boundaries and never
/// splitting a `\r\n` pair.
pub(crate) fn chunk_end(text: &str, start: usize, target: usize) -> usize {
    let mut end = (start + target).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let bytes = text.as_bytes();
    if end < text.len() && end > 0 && bytes[end - 1] == b'\r' && bytes[end] == b'\n' {
        end += 1;
    }
    end
}

/// Inserts `text` at `char_idx`. Returns a new right sibling when this node had
/// to split, which the caller must place after `node`.
///
/// `text` must not exceed [`MAX_LEAF_BYTES`]; larger insertions are chunked by
/// the caller so that one split is always enough.
pub(crate) fn insert(node: &mut Arc<Node>, char_idx: usize, text: &str) -> Option<Arc<Node>> {
    debug_assert!(text.len() <= MAX_LEAF_BYTES);
    match Arc::make_mut(node) {
        Node::Leaf { text: chunk, info } => {
            let byte = byte_of_char(chunk, info, char_idx);
            chunk.insert_str(byte, text);
            if chunk.len() <= MAX_LEAF_BYTES {
                *info = TextInfo::from_str(chunk);
                None
            } else {
                let split = split_point(chunk);
                let tail = chunk.split_off(split);
                chunk.shrink_to(MAX_LEAF_BYTES);
                *info = TextInfo::from_str(chunk);
                Some(Arc::new(Node::leaf(tail)))
            }
        }
        Node::Internal { children, info } => {
            let (index, offset) = child_for_insert(children, char_idx);
            if let Some(extra) = insert(&mut children[index], offset, text) {
                children.insert(index + 1, extra);
            }
            if children.len() <= MAX_CHILDREN {
                *info = sum_info(children);
                None
            } else {
                let tail = children.split_off(children.len() / 2);
                *info = sum_info(children);
                Some(Arc::new(Node::internal(tail)))
            }
        }
    }
}

/// Splits an over-long chunk near its middle, on a character boundary and never
/// between `\r` and `\n`.
fn split_point(text: &str) -> usize {
    let mut split = text.len() / 2;
    while split < text.len() && !text.is_char_boundary(split) {
        split += 1;
    }
    let bytes = text.as_bytes();
    if split < text.len() && split > 0 && bytes[split - 1] == b'\r' && bytes[split] == b'\n' {
        split += 1;
    }
    if split == 0 {
        // Degenerate case: a single character wider than half the chunk.
        text.char_indices().nth(1).map(|(b, _)| b).unwrap_or(text.len())
    } else {
        split
    }
}

/// Picks the child that an insertion at `char_idx` belongs to. At a boundary the
/// left child wins, so consecutive typing keeps extending the same chunk.
fn child_for_insert(children: &[Arc<Node>], char_idx: usize) -> (usize, usize) {
    let mut remaining = char_idx;
    for (index, child) in children.iter().enumerate() {
        let chars = child.len_chars();
        if remaining <= chars {
            return (index, remaining);
        }
        remaining -= chars;
    }
    let last = children.len() - 1;
    (last, children[last].len_chars())
}

/// Removes the character range `[start, end)`.
pub(crate) fn remove(node: &mut Arc<Node>, start: usize, end: usize) {
    if start >= end {
        return;
    }
    match Arc::make_mut(node) {
        Node::Leaf { text, info } => {
            let from = byte_of_char(text, info, start);
            let to = byte_of_char(text, info, end);
            text.replace_range(from..to, "");
            *info = TextInfo::from_str(text);
        }
        Node::Internal { children, info } => {
            let mut offset = 0;
            let mut fully_covered: Vec<usize> = Vec::new();
            // Indexed rather than iterated: the body recurses into a child
            // mutably, which an immutable iterator would forbid.
            #[allow(clippy::needless_range_loop)]
            for index in 0..children.len() {
                let chars = children[index].len_chars();
                let child_start = offset;
                let child_end = offset + chars;
                offset = child_end;

                if child_end <= start {
                    continue;
                }
                if child_start >= end {
                    break;
                }
                let local_start = start.saturating_sub(child_start);
                let local_end = (end - child_start).min(chars);
                if local_start == 0 && local_end == chars {
                    fully_covered.push(index);
                } else {
                    remove(&mut children[index], local_start, local_end);
                }
            }
            for index in fully_covered.into_iter().rev() {
                children.remove(index);
            }
            if children.is_empty() {
                children.push(Arc::new(Node::empty()));
            }
            rebalance(children);
            *info = sum_info(children);
        }
    }
}

fn is_underfull(node: &Node) -> bool {
    match node {
        Node::Leaf { text, .. } => text.len() < MAX_LEAF_BYTES / 3,
        Node::Internal { children, .. } => children.len() < MAX_CHILDREN / 2,
    }
}

/// Merges or redistributes under-filled siblings so the tree does not degrade
/// into a long chain of nearly empty nodes after large deletions.
fn rebalance(children: &mut Vec<Arc<Node>>) {
    let mut index = 0;
    while index < children.len() {
        if children.len() < 2 || !is_underfull(&children[index]) {
            index += 1;
            continue;
        }
        let left = if index + 1 < children.len() { index } else { index - 1 };
        let right = left + 1;
        let right_node = children.remove(right);
        let produced_sibling = merge_or_redistribute(&mut children[left], right_node);
        if let Some(sibling) = produced_sibling {
            children.insert(right, sibling);
            // Both nodes are now adequately filled; move past them.
            index = right + 1;
        } else {
            // The merged node may still be under-filled; each merge shrinks the
            // child list, so re-checking it cannot loop forever.
            index = left;
        }
    }
}

/// Merges `right` into `left`. If the combined content does not fit in one
/// node, splits it evenly and returns the new right sibling.
fn merge_or_redistribute(left: &mut Arc<Node>, right: Arc<Node>) -> Option<Arc<Node>> {
    match (Arc::make_mut(left), right.as_ref()) {
        (Node::Leaf { text, info }, Node::Leaf { text: right_text, .. }) => {
            text.push_str(right_text);
            if text.len() <= MAX_LEAF_BYTES {
                *info = TextInfo::from_str(text);
                None
            } else {
                let split = split_point(text);
                let tail = text.split_off(split);
                text.shrink_to(MAX_LEAF_BYTES);
                *info = TextInfo::from_str(text);
                Some(Arc::new(Node::leaf(tail)))
            }
        }
        (Node::Internal { children, info }, Node::Internal { children: right_children, .. }) => {
            children.extend(right_children.iter().cloned());
            if children.len() <= MAX_CHILDREN {
                *info = sum_info(children);
                None
            } else {
                let tail = children.split_off(children.len() / 2);
                *info = sum_info(children);
                Some(Arc::new(Node::internal(tail)))
            }
        }
        // Siblings are always at the same depth, so mixed pairs cannot occur.
        _ => Some(right),
    }
}

/// Removes redundant single-child roots left behind by deletions.
pub(crate) fn collapse_root(root: &mut Arc<Node>) {
    loop {
        let single_child = match root.as_ref() {
            Node::Internal { children, .. } if children.len() == 1 => Some(children[0].clone()),
            _ => None,
        };
        match single_child {
            Some(child) => *root = child,
            None => break,
        }
    }
}

/// Depth-first iterator over the chunks of a rope.
pub(crate) struct Chunks<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> Chunks<'a> {
    pub(crate) fn new(root: &'a Node) -> Self {
        Chunks { stack: vec![root] }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        while let Some(node) = self.stack.pop() {
            match node {
                Node::Leaf { text, .. } => {
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
                Node::Internal { children, .. } => {
                    for child in children.iter().rev() {
                        self.stack.push(child.as_ref());
                    }
                }
            }
        }
        None
    }
}
