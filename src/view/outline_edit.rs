//! Editing the bookmark tree. Items are addressed by their path of child indices.

use crate::engine::OutlineItem;

pub type Path = Vec<usize>;

fn siblings_mut<'a>(items: &'a mut Vec<OutlineItem>, parent: &[usize]) -> Option<&'a mut Vec<OutlineItem>> {
    let mut list = items;
    for &i in parent {
        list = &mut list.get_mut(i)?.children;
    }
    Some(list)
}

pub fn get<'a>(items: &'a [OutlineItem], path: &[usize]) -> Option<&'a OutlineItem> {
    let (first, rest) = path.split_first()?;
    let item = items.get(*first)?;
    if rest.is_empty() { Some(item) } else { get(&item.children, rest) }
}

pub fn get_mut<'a>(items: &'a mut [OutlineItem], path: &[usize]) -> Option<&'a mut OutlineItem> {
    let (first, rest) = path.split_first()?;
    let item = items.get_mut(*first)?;
    if rest.is_empty() { Some(item) } else { get_mut(&mut item.children, rest) }
}

/// Inserts `item` right after `after` (or at the end of the top level). Returns its path.
pub fn insert_after(items: &mut Vec<OutlineItem>, after: Option<&[usize]>, item: OutlineItem) -> Path {
    match after.and_then(|p| p.split_last()) {
        Some((&last, parent)) => match siblings_mut(items, parent) {
            Some(list) if last < list.len() => {
                list.insert(last + 1, item);
                let mut p = parent.to_vec();
                p.push(last + 1);
                p
            }
            _ => {
                items.push(item);
                vec![items.len() - 1]
            }
        },
        None => {
            items.push(item);
            vec![items.len() - 1]
        }
    }
}

pub fn remove(items: &mut Vec<OutlineItem>, path: &[usize]) -> Option<OutlineItem> {
    let (&last, parent) = path.split_last()?;
    let list = siblings_mut(items, parent)?;
    (last < list.len()).then(|| list.remove(last))
}

/// Moves an item up (`-1`) or down (`1`) among its siblings. Returns its new path.
pub fn shift(items: &mut Vec<OutlineItem>, path: &[usize], by: isize) -> Option<Path> {
    let (&last, parent) = path.split_last()?;
    let list = siblings_mut(items, parent)?;
    let to = last.checked_add_signed(by).filter(|&t| t < list.len())?;
    list.swap(last, to);
    let mut p = parent.to_vec();
    p.push(to);
    Some(p)
}

/// Makes an item the last child of the sibling above it.
pub fn indent(items: &mut Vec<OutlineItem>, path: &[usize]) -> Option<Path> {
    let (&last, parent) = path.split_last()?;
    if last == 0 {
        return None;
    }
    let item = remove(items, path)?;
    let prev = siblings_mut(items, parent)?.get_mut(last - 1)?;
    prev.children.push(item);
    let mut p = parent.to_vec();
    p.extend([last - 1, prev.children.len() - 1]);
    Some(p)
}

/// Moves an item out of its parent, right after the parent.
pub fn outdent(items: &mut Vec<OutlineItem>, path: &[usize]) -> Option<Path> {
    if path.len() < 2 {
        return None;
    }
    let item = remove(items, path)?;
    let parent = &path[..path.len() - 1];
    Some(insert_after(items, Some(parent), item))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, children: Vec<OutlineItem>) -> OutlineItem {
        OutlineItem { title: title.into(), page: None, children, origin: None }
    }

    fn titles(items: &[OutlineItem]) -> String {
        items
            .iter()
            .map(
                |i| {
                    if i.children.is_empty() {
                        i.title.clone()
                    } else {
                        format!("{}({})", i.title, titles(&i.children))
                    }
                },
            )
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn edit_the_tree() {
        let mut t = vec![item("A", vec![item("A1", vec![])]), item("B", vec![]), item("C", vec![])];
        assert_eq!(insert_after(&mut t, Some(&[0, 0]), item("A2", vec![])), vec![0, 1]);
        assert_eq!(titles(&t), "A(A1 A2) B C");
        assert_eq!(indent(&mut t, &[2]), Some(vec![1, 0]));
        assert_eq!(titles(&t), "A(A1 A2) B(C)");
        assert_eq!(outdent(&mut t, &[0, 0]), Some(vec![1]));
        assert_eq!(titles(&t), "A(A2) A1 B(C)");
        assert_eq!(shift(&mut t, &[1], -1), Some(vec![0]));
        assert_eq!(titles(&t), "A1 A(A2) B(C)");
        assert_eq!(shift(&mut t, &[0], -1), None, "already first");
        assert_eq!(indent(&mut t, &[0]), None, "nothing above to indent under");
        assert_eq!(remove(&mut t, &[2, 0]).map(|i| i.title), Some("C".into()));
        assert_eq!(titles(&t), "A1 A(A2) B");
        get_mut(&mut t, &[1, 0]).unwrap().title = "X".into();
        assert_eq!(get(&t, &[1, 0]).unwrap().title, "X");
    }
}
