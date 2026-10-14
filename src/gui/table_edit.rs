//! The Diagram tab's table editor: a dialog that edits one table's name and
//! columns as a grid and produces the whole changed schema.

use eframe::egui::{self, Pos2, RichText};
use simple_database::{format, parser};
use simple_database::schema::{RefAction, Schema, Table, note_rename};
use simple_database::validate::{self, Severity};

const TYPES: [&str; 15] = [
    "int", "bigint", "smallint", "real", "decimal(10,2)", "bool", "text", "varchar(255)", "date", "time", "timestamp", "blob", "uuid", "json",
    "enum('a', 'b')",
];
const ACTIONS: [RefAction; 5] = [RefAction::NoAction, RefAction::Cascade, RefAction::SetNull, RefAction::SetDefault, RefAction::Restrict];

/// One column as the grid edits it; defaults and checks in DSL spelling.
#[derive(Clone, Default)]
struct Row {
    /// Name in the schema before editing; `None` for an added column.
    original: Option<String>,
    name: String,
    ty: String,
    pk: bool,
    not_null: bool,
    unique: bool,
    auto_increment: bool,
    default: String,
    check: String,
    /// `table.column`, or empty.
    references: String,
    on_delete: Option<RefAction>,
    on_update: Option<RefAction>,
    comment: String,
}

pub struct TableEditor {
    /// Table being edited; `None` while creating one.
    original: Option<String>,
    /// Where a new table goes on the canvas, in world coordinates.
    pub place: Option<Pos2>,
    name: String,
    rows: Vec<Row>,
    /// Composite key; edited in the schema text only, shown for reference.
    composite_pk: Vec<String>,
    /// Multi-column foreign keys as the DSL writes them; likewise only shown.
    foreign_keys: Vec<String>,
    comment: String,
    error: Option<String>,
}

#[derive(Debug)]
pub enum Outcome {
    /// The changed schema, the table's final name and its old name if renamed.
    Apply { schema: Schema, table: String, renamed: Option<(String, String)> },
    Cancel,
}

impl TableEditor {
    pub fn edit(t: &Table) -> Self {
        let rows = t
            .columns
            .iter()
            .map(|c| Row {
                original: Some(c.name.clone()),
                name: c.name.clone(),
                ty: format::type_text(&c.name, &c.data_type),
                pk: c.primary_key,
                not_null: !c.nullable,
                unique: c.unique,
                auto_increment: c.auto_increment,
                default: c.default.as_ref().map(ToString::to_string).unwrap_or_default(),
                check: c.check.clone().unwrap_or_default(),
                references: c.references.as_ref().map(|fk| format!("{}.{}", fk.table, fk.column)).unwrap_or_default(),
                on_delete: c.references.as_ref().map(|fk| fk.on_delete),
                on_update: c.references.as_ref().map(|fk| fk.on_update),
                comment: c.comment.clone().unwrap_or_default(),
            })
            .collect();
        let foreign_keys = t.foreign_keys.iter().map(|fk| format!("({}) -> {} ({})", fk.columns.join(", "), fk.table, fk.ref_columns.join(", "))).collect();
        TableEditor {
            original: Some(t.name.clone()),
            place: None,
            name: t.name.clone(),
            rows,
            composite_pk: t.composite_pk.clone(),
            foreign_keys,
            comment: t.comment.clone().unwrap_or_default(),
            error: None,
        }
    }

    /// A new table with an `id` key, named so it doesn't clash with `schema`.
    pub fn create(schema: &Schema, place: Pos2) -> Self {
        let name = (1..).map(|i| if i == 1 { "new_table".to_string() } else { format!("new_table_{i}") }).find(|n| schema.table(n).is_none()).unwrap();
        let id = Row { name: "id".into(), ty: "int".into(), pk: true, auto_increment: true, ..Row::default() };
        TableEditor { original: None, place: Some(place), name, rows: vec![id], composite_pk: vec![], foreign_keys: vec![], comment: String::new(), error: None }
    }

    /// Start with an empty column row focused at the end.
    pub fn with_new_column(mut self) -> Self {
        self.rows.push(Row::default());
        self
    }

    pub fn title(&self) -> String {
        match &self.original {
            Some(n) => format!("Edit table {n}"),
            None => "New table".into(),
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, schema: &Schema) -> Option<Outcome> {
        let mut outcome = None;
        ui.heading(self.title());
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(220.0));
            ui.label("Comment:");
            ui.add(egui::TextEdit::singleline(&mut self.comment).desired_width(260.0).hint_text("what the table holds"));
        });
        if !self.composite_pk.is_empty() {
            ui.label(RichText::new(format!("Composite key: ({})", self.composite_pk.join(", "))).weak());
        }
        for fk in &self.foreign_keys {
            ui.label(RichText::new(format!("Foreign key: {fk}")).weak());
        }
        ui.add_space(6.0);

        let targets = self.reference_targets(schema);
        let mut remove = None;
        let mut swap = None;
        let last = self.rows.len().saturating_sub(1);
        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            egui::Grid::new("table_edit_grid").striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
                for h in ["Column", "Type", "PK", "Not null", "Unique", "Auto inc", "Default", "Check", "Comment", "References", "On delete", ""] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();
                for (i, row) in self.rows.iter_mut().enumerate() {
                    ui.add_sized([140.0, 20.0], egui::TextEdit::singleline(&mut row.name).hint_text("name"));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.add(egui::TextEdit::singleline(&mut row.ty).desired_width(100.0).hint_text("type"));
                        ui.menu_button("⏷", |ui| {
                            for t in TYPES {
                                if ui.button(t).clicked() {
                                    row.ty = t.to_string();
                                    ui.close();
                                }
                            }
                        });
                    });
                    ui.checkbox(&mut row.pk, "");
                    ui.add_enabled_ui(!row.pk, |ui| {
                        let mut not_null = row.not_null || row.pk;
                        ui.checkbox(&mut not_null, "");
                        if !row.pk {
                            row.not_null = not_null;
                        }
                    });
                    ui.checkbox(&mut row.unique, "");
                    ui.checkbox(&mut row.auto_increment, "");
                    ui.add_sized([110.0, 20.0], egui::TextEdit::singleline(&mut row.default).hint_text("0, 'x', now"));
                    ui.add_sized([110.0, 20.0], egui::TextEdit::singleline(&mut row.check).hint_text("expression"));
                    ui.add_sized([140.0, 20.0], egui::TextEdit::singleline(&mut row.comment).hint_text("description"));
                    let shown = if row.references.is_empty() { "—" } else { row.references.as_str() };
                    egui::ComboBox::from_id_salt(("ref", i)).selected_text(shown).width(150.0).show_ui(ui, |ui| {
                        if ui.selectable_label(row.references.is_empty(), "—").clicked() {
                            row.references.clear();
                        }
                        for (target, ty) in &targets {
                            if ui.selectable_label(row.references == *target, target).clicked() {
                                row.references = target.clone();
                                if row.ty.trim().is_empty() {
                                    row.ty = ty.clone();
                                }
                            }
                        }
                    });
                    ui.add_enabled_ui(!row.references.is_empty(), |ui| {
                        let mut action = row.on_delete.unwrap_or(RefAction::NoAction);
                        egui::ComboBox::from_id_salt(("on_delete", i)).selected_text(action.dsl()).width(100.0).show_ui(ui, |ui| {
                            for a in ACTIONS {
                                ui.selectable_value(&mut action, a, a.dsl());
                            }
                        });
                        row.on_delete = Some(action);
                    });
                    ui.horizontal(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new("⬆").small()).on_hover_text("Move up").clicked() {
                            swap = Some(i - 1);
                        }
                        if ui.add_enabled(i < last, egui::Button::new("⬇").small()).on_hover_text("Move down").clicked() {
                            swap = Some(i);
                        }
                        if ui.button("✖").on_hover_text("Delete column").clicked() {
                            remove = Some(i);
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if let Some(i) = swap {
            self.rows.swap(i, i + 1);
        }
        if let Some(i) = remove {
            self.rows.remove(i);
        }
        if ui.button("+ Add column").clicked() {
            self.rows.push(Row::default());
        }
        ui.label(RichText::new("Defaults and checks use the schema syntax: 0, 'text', true, now, (expression).").weak().small());

        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Apply").strong()).clicked() {
                match self.build(schema) {
                    Ok(o) => outcome = Some(o),
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.button("Cancel").clicked() {
                outcome = Some(Outcome::Cancel);
            }
        });
        outcome
    }

    /// Columns a foreign key can point at, with their types; this table's own
    /// columns as currently edited (for self references) come first.
    fn reference_targets(&self, schema: &Schema) -> Vec<(String, String)> {
        let own = self.rows.iter().filter(|r| !r.name.trim().is_empty()).map(|r| (format!("{}.{}", self.name.trim(), r.name.trim()), r.ty.trim().to_string()));
        let others = schema
            .tables
            .iter()
            .filter(|t| Some(&t.name) != self.original.as_ref())
            .flat_map(|t| t.columns.iter().map(move |c| (format!("{}.{}", t.name, c.name), format::type_text(&c.name, &c.data_type))));
        own.chain(others).collect()
    }

    /// A self reference still written with the table's or a column's old name,
    /// pointed at the new ones.
    fn own_reference(&self, target: &str, name: &str) -> String {
        let Some((table, column)) = target.rsplit_once('.') else { return target.to_string() };
        if Some(table) != self.original.as_deref() && table != name {
            return target.to_string();
        }
        let column = match self.rows.iter().find(|r| r.name.trim() == column) {
            Some(r) => r.name.trim(),
            None => self.rows.iter().find(|r| r.original.as_deref() == Some(column)).map_or(column, |r| r.name.trim()),
        };
        format!("{name}.{column}")
    }

    /// `schema` with this table replaced by the edited one. Renames carry over
    /// to keys, indexes and foreign keys; errors the edit introduces are refused.
    fn build(&self, schema: &Schema) -> Result<Outcome, String> {
        let name = self.name.trim().to_string();
        let rows: Vec<&Row> = self.rows.iter().filter(|r| !(r.name.trim().is_empty() && r.ty.trim().is_empty())).collect();
        if rows.is_empty() {
            return Err("a table needs at least one column".into());
        }
        if Some(&name) != self.original.as_ref() && schema.table(&name).is_some() {
            return Err(format!("there is already a table called '{name}'"));
        }
        let mut lines = vec![format!("table {name} {{")];
        for (i, r) in rows.iter().enumerate() {
            let (col, ty) = (r.name.trim(), r.ty.trim());
            if col.is_empty() || ty.is_empty() {
                return Err(format!("column {} needs a name and a type", i + 1));
            }
            if r.pk && !self.composite_pk.is_empty() {
                return Err(format!("'{col}': the table already has a composite key ({})", self.composite_pk.join(", ")));
            }
            let mut line = format!("  {col} {ty}");
            for (on, word) in [(r.pk, "pk"), (r.auto_increment, "autoincrement"), (r.unique, "unique"), (r.not_null && !r.pk, "not null")] {
                if on {
                    line = format!("{line} {word}");
                }
            }
            if !r.default.trim().is_empty() {
                line = format!("{line} default {}", r.default.trim());
            }
            if !r.check.trim().is_empty() {
                let check = r.check.trim();
                let wrapped = check.starts_with('(') && check.ends_with(')');
                line = if wrapped { format!("{line} check {check}") } else { format!("{line} check ({check})") };
            }
            if !r.references.is_empty() {
                line = format!("{line} -> {}", self.own_reference(&r.references, &name));
                for (action, word) in [(r.on_delete, "delete"), (r.on_update, "update")] {
                    if let Some(a) = action.filter(|a| *a != RefAction::NoAction) {
                        line = format!("{line} on {word} {}", a.dsl());
                    }
                }
            }
            if !r.comment.trim().is_empty() {
                line = format!("{line} comment '{}'", r.comment.trim().replace('\'', "''"));
            }
            lines.push(line);
        }
        lines.push("}".into());
        let parsed = parser::parse(&lines.join("\n")).map_err(|e| match e.line {
            n if n >= 2 && n - 2 < rows.len() => format!("column '{}': {}", rows[n - 2].name.trim(), e.message),
            _ => e.message,
        })?;
        let columns = parsed.tables.into_iter().next().map(|t| t.columns).unwrap_or_default();
        let comment = Some(self.comment.trim().to_string()).filter(|c| !c.is_empty());

        let mut out = schema.clone();
        let mut renamed = None;
        match &self.original {
            Some(old) => {
                let kept: Vec<&str> = rows.iter().filter_map(|r| r.original.as_deref()).collect();
                let dropped: Vec<String> =
                    out.table(old).map(|t| t.columns.iter().filter(|c| !kept.contains(&c.name.as_str())).map(|c| c.name.clone()).collect()).unwrap_or_default();
                for c in dropped {
                    out.drop_column(old, &c);
                }
                // Through temporary names, so swapping two names works.
                let moves: Vec<(&str, &str)> = rows.iter().filter_map(|r| Some((r.original.as_deref()?, r.name.trim()))).filter(|(o, n)| o != n).collect();
                for (i, (from, _)) in moves.iter().enumerate() {
                    out.rename_column(old, from, &format!("\0{i}"));
                }
                for (i, (_, to)) in moves.iter().enumerate() {
                    out.rename_column(old, &format!("\0{i}"), to);
                }
                if *old != name {
                    out.rename_table(old, &name);
                    renamed = Some((old.clone(), name.clone()));
                }
                // Self references were written with the new names already.
                if let Some(t) = out.tables.iter_mut().find(|t| t.name == name) {
                    t.columns = columns;
                    t.comment = comment;
                    // Earlier names, so that a migration renames instead of dropping.
                    let before = schema.table(old);
                    note_rename(&mut t.was, old, &name);
                    for (c, r) in t.columns.iter_mut().zip(&rows) {
                        if let Some(o) = &r.original
                            && let Some(oc) = before.and_then(|b| b.column(o))
                        {
                            c.was = oc.was.clone();
                            note_rename(&mut c.was, o, &c.name);
                        }
                    }
                }
            }
            None => out.tables.push(Table { columns, comment, ..Table::new(name.clone()) }),
        }

        let before: Vec<(String, String)> = validate::validate(schema).into_iter().map(|i| (i.location, i.message)).collect();
        let new_errors: Vec<String> = validate::validate(&out)
            .into_iter()
            .filter(|i| i.severity == Severity::Error && !before.contains(&(i.location.clone(), i.message.clone())))
            .map(|i| format!("{}: {}", i.location, i.message))
            .collect();
        if !new_errors.is_empty() {
            return Err(new_errors.join("; "));
        }
        Ok(Outcome::Apply { schema: out, table: name, renamed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "table users {\n  id int pk\n  boss int -> users.id\n  email text unique\n  index (email)\n}\n\ntable posts {\n  id int pk\n  author int -> users.id\n}\n";

    fn apply(e: &TableEditor, s: &Schema) -> (Schema, Option<(String, String)>) {
        match e.build(s) {
            Ok(Outcome::Apply { schema, renamed, .. }) => (schema, renamed),
            Ok(Outcome::Cancel) => unreachable!(),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn renames_follow_through_the_schema() {
        let s = parser::parse(SRC).unwrap();
        let mut e = TableEditor::edit(s.table("users").unwrap());
        assert_eq!(apply(&e, &s).0, s, "no edits, no changes");

        e.name = "people".into();
        e.rows[0].name = "pid".into();
        e.rows[2].name = "mail".into();
        let (out, renamed) = apply(&e, &s);
        assert_eq!(renamed, Some(("users".into(), "people".into())));
        let people = out.table("people").unwrap();
        assert_eq!(people.was, ["users"]);
        let was: Vec<&[String]> = people.columns.iter().map(|c| c.was.as_slice()).collect();
        assert_eq!(was, [&["id".to_string()][..], &[], &["email".to_string()]]);
        // Renamed again before a push, every earlier name is kept; renamed back, none.
        let mut again = TableEditor::edit(people);
        again.name = "persons".into();
        assert_eq!(apply(&again, &out).0.table("persons").unwrap().was, ["users", "people"]);
        again.name = "users".into();
        again.rows[0].name = "id".into();
        let back = apply(&again, &out).0;
        assert_eq!(back.table("users").unwrap().was, ["people"]);
        assert!(back.table("users").unwrap().columns[0].was == ["pid"]);
        let boss = people.column("boss").unwrap().references.as_ref().unwrap();
        assert_eq!((boss.table.as_str(), boss.column.as_str()), ("people", "pid"));
        assert_eq!(people.indexes[0].columns, vec!["mail"]);
        let author = out.table("posts").unwrap().column("author").unwrap().references.as_ref().unwrap();
        assert_eq!((author.table.as_str(), author.column.as_str()), ("people", "pid"));
    }

    #[test]
    fn drops_and_refuses_new_errors() {
        let s = parser::parse(SRC).unwrap();
        let mut e = TableEditor::edit(s.table("users").unwrap());
        e.rows.remove(2);
        assert!(apply(&e, &s).0.table("users").unwrap().indexes.is_empty());

        e.rows[1].references = "posts.author".into();
        assert!(e.build(&s).is_err(), "posts.author is not a key");
        e.rows[1].references.clear();
        e.rows[1].default = "'unterminated".into();
        assert!(e.build(&s).unwrap_err().starts_with("column 'boss'"));

        let mut n = TableEditor::create(&s, Pos2::ZERO).with_new_column();
        assert!(n.build(&s).is_ok(), "the empty trailing row is ignored");
        n.name = "posts".into();
        assert!(n.build(&s).is_err());
    }
}
