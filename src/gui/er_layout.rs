//! Where the ER diagram's table boxes go. `layout` arranges every table: each
//! group of related tables in layers by foreign-key depth (parents left of the
//! tables that reference them), ordered to cut line crossings, tall layers
//! wrapped into several columns, and the groups packed side by side with the
//! unrelated tables in a grid at the end. `place` finds free space for one new
//! box, and `separate` pushes overlapping boxes apart.

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use simple_database::schema::Schema;
use std::collections::{HashMap, HashSet};

pub const COL_GAP: f32 = 90.0;
pub const ROW_GAP: f32 = 40.0;
/// Between two columns of one wrapped layer, and between packed groups.
const WRAP_GAP: f32 = 50.0;
const GROUP_GAP: f32 = 120.0;

/// The relationships between the tables that have a size.
struct Graph<'a> {
    /// Schema order.
    names: Vec<&'a str>,
    parents: HashMap<&'a str, Vec<&'a str>>,
    neighbors: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> Graph<'a> {
    fn new(schema: &'a Schema, sizes: &HashMap<String, Vec2>) -> Self {
        let names: Vec<&str> = schema.tables.iter().map(|t| t.name.as_str()).filter(|n| sizes.contains_key(*n)).collect();
        let mut parents: HashMap<&str, Vec<&str>> = names.iter().map(|n| (*n, vec![])).collect();
        let mut neighbors = parents.clone();
        for t in schema.tables.iter().filter(|t| sizes.contains_key(&t.name)) {
            for fk in t.all_foreign_keys() {
                let Some(p) = schema.table(&fk.table).map(|p| p.name.as_str()).filter(|p| *p != t.name && sizes.contains_key(*p)) else {
                    continue;
                };
                let child = t.name.as_str();
                if !parents[child].contains(&p) {
                    parents.get_mut(child).unwrap().push(p);
                    neighbors.get_mut(child).unwrap().push(p);
                    neighbors.get_mut(p).unwrap().push(child);
                }
            }
        }
        Graph { names, parents, neighbors }
    }

    /// Longest chain of references above each table; a cycle is cut where it closes.
    fn depths(&self) -> HashMap<&'a str, usize> {
        fn visit<'a>(g: &Graph<'a>, n: &'a str, memo: &mut HashMap<&'a str, usize>, stack: &mut Vec<&'a str>) -> usize {
            if let Some(&d) = memo.get(n) {
                return d;
            }
            if stack.contains(&n) {
                return 0;
            }
            stack.push(n);
            let d = g.parents[n].iter().map(|p| visit(g, p, memo, stack) + 1).max().unwrap_or(0);
            stack.pop();
            memo.insert(n, d);
            d
        }
        let mut memo = HashMap::new();
        for n in &self.names {
            visit(self, n, &mut memo, &mut Vec::new());
        }
        memo
    }

    /// Connected groups in schema order, largest first.
    fn groups(&self) -> Vec<Vec<&'a str>> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        for &start in &self.names {
            if !seen.insert(start) {
                continue;
            }
            let mut group = vec![start];
            let mut i = 0;
            while i < group.len() {
                for &n in &self.neighbors[group[i]] {
                    if seen.insert(n) {
                        group.push(n);
                    }
                }
                i += 1;
            }
            let order: HashMap<&str, usize> = self.names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
            group.sort_by_key(|n| order[n]);
            out.push(group);
        }
        out.sort_by_key(|g| std::cmp::Reverse(g.len()));
        out
    }
}

/// Top-left corners for every table in `sizes` (world units).
pub fn layout(schema: &Schema, sizes: &HashMap<String, Vec2>) -> HashMap<String, Pos2> {
    let graph = Graph::new(schema, sizes);
    let depths = graph.depths();
    let mut blocks: Vec<(HashMap<String, Pos2>, Vec2)> = Vec::new();
    let mut loners: Vec<&str> = Vec::new();
    for group in graph.groups() {
        if group.len() == 1 {
            loners.push(group[0]);
        } else {
            blocks.push(layout_group(&graph, &group, &depths, sizes));
        }
    }
    if !loners.is_empty() {
        let items: Vec<(HashMap<String, Pos2>, Vec2)> =
            loners.iter().map(|n| (HashMap::from([(n.to_string(), Pos2::ZERO)]), sizes[*n])).collect();
        blocks.push(pack(items, ROW_GAP));
    }
    pack(blocks, GROUP_GAP).0
}

/// Shelf-pack blocks (positions relative to their own top-left, and size) into
/// rows about as wide as the result is tall, scaled to a landscape screen.
fn pack(blocks: Vec<(HashMap<String, Pos2>, Vec2)>, gap: f32) -> (HashMap<String, Pos2>, Vec2) {
    let area: f32 = blocks.iter().map(|(_, s)| (s.x + gap) * (s.y + gap)).sum();
    let widest = blocks.iter().map(|(_, s)| s.x).fold(0.0, f32::max);
    let row_width = widest.max((area * 1.6).sqrt());
    let mut out = HashMap::new();
    let (mut x, mut y, mut shelf, mut width) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for (positions, size) in blocks {
        if x > 0.0 && x + size.x > row_width {
            x = 0.0;
            y += shelf + gap;
            shelf = 0.0;
        }
        for (name, p) in positions {
            out.insert(name, p + vec2(x, y));
        }
        width = width.max(x + size.x);
        shelf = shelf.max(size.y);
        x += size.x + gap;
    }
    (out, vec2(width, y + shelf))
}

/// One connected group, normalized to start at (0, 0), and its size.
fn layout_group(g: &Graph, group: &[&str], depths: &HashMap<&str, usize>, sizes: &HashMap<String, Vec2>) -> (HashMap<String, Pos2>, Vec2) {
    // Layers by depth, empty ones dropped (a cut cycle can leave gaps).
    let mut levels: Vec<usize> = group.iter().map(|n| depths[n]).collect();
    levels.sort_unstable();
    levels.dedup();
    let mut layers: Vec<Vec<&str>> = levels.iter().map(|d| group.iter().copied().filter(|n| depths[n] == *d).collect()).collect();
    let layer_of: HashMap<&str, usize> = layers.iter().enumerate().flat_map(|(i, l)| l.iter().map(move |n| (*n, i))).collect();

    // Order each layer by the mean rank of its neighbors in the layers already
    // ordered (barycenter heuristic), sweeping right then left a few times.
    let mut rank: HashMap<&str, f32> = HashMap::new();
    fn set_ranks<'a>(layer: &[&'a str], rank: &mut HashMap<&'a str, f32>) {
        for (i, n) in layer.iter().enumerate() {
            rank.insert(n, (i as f32 + 0.5) / layer.len() as f32);
        }
    }
    for l in &layers {
        set_ranks(l, &mut rank);
    }
    for _ in 0..4 {
        for forward in [true, false] {
            let order: Vec<usize> = if forward { (1..layers.len()).collect() } else { (0..layers.len().saturating_sub(1)).rev().collect() };
            for i in order {
                let key = |n: &str| {
                    let near: Vec<f32> = g.neighbors[n]
                        .iter()
                        .filter(|m| if forward { layer_of[*m] < i } else { layer_of[*m] > i })
                        .map(|m| rank[m])
                        .collect();
                    if near.is_empty() { rank[n] } else { near.iter().sum::<f32>() / near.len() as f32 }
                };
                let keys: HashMap<&str, f32> = layers[i].iter().map(|n| (*n, key(n))).collect();
                layers[i].sort_by(|a, b| keys[a].total_cmp(&keys[b]));
                set_ranks(&layers[i], &mut rank);
            }
        }
    }

    // Wrap layers much taller than the group would be if it were square.
    let area: f32 = group.iter().map(|n| (sizes[*n].x + COL_GAP) * (sizes[*n].y + ROW_GAP)).sum();
    let tallest = group.iter().map(|n| sizes[*n].y).fold(0.0, f32::max);
    let limit = tallest.max(area.sqrt() * 1.2);
    let mut columns: Vec<(Vec<&str>, bool)> = Vec::new(); // (tables, starts a new layer)
    for layer in &layers {
        let total: f32 = layer.iter().map(|n| sizes[*n].y + ROW_GAP).sum();
        let parts = (total / limit).ceil().max(1.0);
        let share = total / parts;
        let mut column: Vec<&str> = Vec::new();
        let mut height = 0.0;
        let mut first = true;
        for n in layer {
            let h = sizes[*n].y + ROW_GAP;
            if !column.is_empty() && height + h / 2.0 > share {
                columns.push((std::mem::take(&mut column), first));
                first = false;
                height = 0.0;
            }
            column.push(n);
            height += h;
        }
        columns.push((column, first));
    }

    let mut xs = Vec::new();
    let mut x = 0.0;
    for (i, (column, new_layer)) in columns.iter().enumerate() {
        if i > 0 {
            x += if *new_layer { COL_GAP } else { WRAP_GAP };
        }
        xs.push(x);
        x += column.iter().map(|n| sizes[*n].x).fold(0.0, f32::max);
    }

    // Vertical centers: first stacked, then pulled toward the related tables
    // while each column keeps its order and spacing.
    let mut center: HashMap<&str, f32> = HashMap::new();
    for (column, _) in &columns {
        let total: f32 = column.iter().map(|n| sizes[*n].y + ROW_GAP).sum();
        let mut y = -total / 2.0;
        for n in column {
            center.insert(n, y + sizes[*n].y / 2.0);
            y += sizes[*n].y + ROW_GAP;
        }
    }
    for _ in 0..4 {
        for forward in [true, false] {
            let order: Vec<usize> = if forward { (0..columns.len()).collect() } else { (0..columns.len()).rev().collect() };
            for i in order {
                let column = &columns[i].0;
                let wanted: Vec<f32> = column
                    .iter()
                    .map(|n| {
                        let near: Vec<f32> = g.neighbors[*n].iter().map(|m| center[m]).collect();
                        if near.is_empty() { center[n] } else { near.iter().sum::<f32>() / near.len() as f32 }
                    })
                    .collect();
                for (n, y) in column.iter().zip(spread(column, &wanted, sizes)) {
                    center.insert(n, y);
                }
            }
        }
    }

    let mut out = HashMap::new();
    for ((column, _), x) in columns.iter().zip(&xs) {
        for n in column {
            out.insert(n.to_string(), pos2(*x, center[n] - sizes[*n].y / 2.0));
        }
    }
    normalize(out, sizes)
}

/// Centers as close to `wanted` as the column's order and spacing allow.
fn spread(column: &[&str], wanted: &[f32], sizes: &HashMap<String, Vec2>) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::with_capacity(column.len());
    let mut bottom = f32::NEG_INFINITY;
    for (n, w) in column.iter().zip(wanted) {
        let h = sizes[*n].y;
        let y = (w - h / 2.0).max(bottom + ROW_GAP);
        out.push(y + h / 2.0);
        bottom = y + h;
    }
    // Pushing down only drifts the column; share the displacement both ways.
    let drift = out.iter().zip(wanted).map(|(y, w)| y - w).sum::<f32>() / out.len().max(1) as f32;
    out.iter().map(|y| y - drift).collect()
}

fn normalize(positions: HashMap<String, Pos2>, sizes: &HashMap<String, Vec2>) -> (HashMap<String, Pos2>, Vec2) {
    let Some(bounds) = positions.iter().map(|(n, p)| Rect::from_min_size(*p, sizes[n])).reduce(|a, b| a.union(b)) else {
        return (positions, Vec2::ZERO);
    };
    let shift = bounds.min.to_vec2();
    (positions.into_iter().map(|(n, p)| (n, p - shift)).collect(), bounds.size())
}

/// A free spot for `table` among the boxes already placed: right of the tables
/// it references, else left of the tables referencing it, else below everything.
pub fn place(schema: &Schema, table: &str, sizes: &HashMap<String, Vec2>, positions: &HashMap<String, Pos2>) -> Pos2 {
    let size = sizes.get(table).copied().unwrap_or(Vec2::ZERO);
    let rects: Vec<Rect> = positions.iter().filter(|(n, _)| *n != table).filter_map(|(n, p)| Some(Rect::from_min_size(*p, *sizes.get(n)?))).collect();
    let Some(bounds) = rects.iter().copied().reduce(|a, b| a.union(b)) else { return Pos2::ZERO };
    let placed = |n: &str| positions.get(n).and_then(|p| Some(Rect::from_min_size(*p, *sizes.get(n)?)));
    let parents: Vec<Rect> = schema
        .table(table)
        .map(|t| t.all_foreign_keys().iter().filter(|fk| fk.table != table).filter_map(|fk| placed(&fk.table)).collect())
        .unwrap_or_default();
    let children: Vec<Rect> = schema
        .tables
        .iter()
        .filter(|t| t.name != table && t.all_foreign_keys().iter().any(|fk| fk.table == table))
        .filter_map(|t| placed(&t.name))
        .collect();
    let start = if let Some(r) = parents.iter().copied().reduce(|a, b| a.union(b)) {
        pos2(r.max.x + COL_GAP, r.min.y)
    } else if let Some(r) = children.iter().copied().reduce(|a, b| a.union(b)) {
        pos2(r.min.x - COL_GAP - size.x, r.min.y)
    } else {
        pos2(bounds.min.x, bounds.max.y + GROUP_GAP)
    };
    let free = |p: Pos2| {
        let r = Rect::from_min_size(p, size).expand(ROW_GAP / 2.0 - 1.0);
        !rects.iter().any(|o| o.intersects(r))
    };
    (0..200).map(|i| start + vec2(0.0, i as f32 * ROW_GAP / 2.0)).find(|p| free(*p)).unwrap_or(pos2(start.x, bounds.max.y + ROW_GAP))
}

/// Move overlapping boxes apart, keeping `ROW_GAP / 2` between them. Boxes
/// settle in reading order, each one moving to the nearest free spot next to
/// the boxes already settled; `pinned` boxes settle first and stay put.
/// Returns whether anything moved.
pub fn separate(positions: &mut HashMap<String, Pos2>, sizes: &HashMap<String, Vec2>, pinned: &HashSet<String>) -> bool {
    let margin = ROW_GAP / 2.0;
    let mut names: Vec<String> = positions.keys().filter(|n| sizes.contains_key(*n)).cloned().collect();
    names.sort_by(|a, b| {
        let (pa, pb) = (positions[a], positions[b]);
        pinned.contains(b).cmp(&pinned.contains(a)).then(pa.y.total_cmp(&pb.y)).then(pa.x.total_cmp(&pb.x)).then(a.cmp(b))
    });
    let mut settled: Vec<Rect> = Vec::new();
    let mut moved = false;
    for name in names {
        let size = sizes[&name];
        let p = positions[&name];
        let free = |q: Pos2, settled: &[Rect]| {
            let r = Rect::from_min_size(q, size).expand(margin - 0.01);
            !settled.iter().any(|o| o.intersects(r))
        };
        if pinned.contains(&name) || free(p, &settled) {
            settled.push(Rect::from_min_size(p, size));
            continue;
        }
        let mut candidates: Vec<Pos2> = settled
            .iter()
            .flat_map(|r| {
                [
                    pos2(r.max.x + margin, p.y),
                    pos2(r.min.x - margin - size.x, p.y),
                    pos2(p.x, r.max.y + margin),
                    pos2(p.x, r.min.y - margin - size.y),
                ]
            })
            .collect();
        candidates.sort_by(|a, b| a.distance_sq(p).total_cmp(&b.distance_sq(p)));
        let bottom = settled.iter().map(|r| r.max.y).fold(p.y, f32::max);
        let at = candidates.into_iter().find(|q| free(*q, &settled)).unwrap_or(pos2(p.x, bottom + margin));
        positions.insert(name, at);
        settled.push(Rect::from_min_size(at, size));
        moved = true;
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_database::parser::parse;

    fn sizes_of(schema: &Schema) -> HashMap<String, Vec2> {
        schema.tables.iter().map(|t| (t.name.clone(), vec2(160.0 + t.name.len() as f32 * 7.0, 32.0 + 20.0 * t.columns.len() as f32))).collect()
    }

    fn overlaps(positions: &HashMap<String, Pos2>, sizes: &HashMap<String, Vec2>) -> Vec<(String, String)> {
        let mut out = vec![];
        let names: Vec<&String> = positions.keys().collect();
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                let ra = Rect::from_min_size(positions[*a], sizes[*a]);
                let rb = Rect::from_min_size(positions[*b], sizes[*b]);
                if ra.intersects(rb) {
                    out.push((a.to_string(), b.to_string()));
                }
            }
        }
        out
    }

    /// A star-ish schema: one hub referenced by many tables, chains, a cycle and loners.
    fn big_schema() -> Schema {
        let mut text = String::from("table hub {\n  id int pk\n}\n");
        for i in 0..25 {
            text += &format!("table child{i} {{\n  id int pk\n  hub_id int -> hub.id\n  note text\n}}\n");
        }
        for i in 0..6 {
            text += &format!("table grand{i} {{\n  id int pk\n  child_id int -> child{i}.id\n}}\n");
        }
        text += "table a {\n  id int pk\n  b_id int -> b.id\n}\ntable b {\n  id int pk\n  a_id int -> a.id\n  me int -> b.id\n}\n";
        for i in 0..12 {
            text += &format!("table loner{i} {{\n  id int pk\n  name text\n}}\n");
        }
        parse(&text).unwrap()
    }

    #[test]
    fn layout_places_every_table_without_overlaps() {
        for schema in [parse(simple_database::STARTER_SCHEMA).unwrap(), big_schema()] {
            let sizes = sizes_of(&schema);
            let positions = layout(&schema, &sizes);
            assert_eq!(positions.len(), schema.tables.len());
            assert_eq!(overlaps(&positions, &sizes), vec![]);
        }
    }

    #[test]
    fn parents_are_left_of_their_children() {
        let schema = big_schema();
        let sizes = sizes_of(&schema);
        let p = layout(&schema, &sizes);
        for i in 0..6 {
            assert!(p["hub"].x < p[&format!("child{i}")].x);
            assert!(p[&format!("child{i}")].x < p[&format!("grand{i}")].x);
        }
    }

    #[test]
    fn tall_layers_wrap_into_several_columns() {
        let schema = big_schema();
        let sizes = sizes_of(&schema);
        let p = layout(&schema, &sizes);
        let xs: HashSet<i64> = (0..25).map(|i| p[&format!("child{i}")].x as i64).collect();
        assert!(xs.len() > 1, "25 children should not stack in one column");
        let bounds = p.iter().map(|(n, q)| Rect::from_min_size(*q, sizes[n])).reduce(|a, b| a.union(b)).unwrap();
        assert!(bounds.height() < bounds.width() * 2.0, "{bounds:?}");
    }

    #[test]
    fn place_finds_free_space_next_to_the_parent() {
        let schema = big_schema();
        let sizes = sizes_of(&schema);
        let mut p = layout(&schema, &sizes);
        p.remove("grand3");
        let at = place(&schema, "grand3", &sizes, &p);
        assert!(at.x > p["child3"].x);
        p.insert("grand3".into(), at);
        assert_eq!(overlaps(&p, &sizes), vec![]);
    }

    #[test]
    fn separate_removes_overlaps_and_keeps_pinned_boxes() {
        let schema = big_schema();
        let sizes = sizes_of(&schema);
        let mut p: HashMap<String, Pos2> = schema.tables.iter().enumerate().map(|(i, t)| (t.name.clone(), pos2((i % 5) as f32 * 30.0, (i / 5) as f32 * 25.0))).collect();
        let hub = p["hub"];
        assert!(separate(&mut p, &sizes, &HashSet::from(["hub".to_string()])));
        assert_eq!(overlaps(&p, &sizes), vec![]);
        assert_eq!(p["hub"], hub);
        assert!(!separate(&mut p, &sizes, &HashSet::new()));
    }
}
