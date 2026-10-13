//! In-memory representation of a database schema, independent of any SQL dialect.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    Int,
    BigInt,
    SmallInt,
    Real,
    Decimal(u32, u32),
    Bool,
    Text,
    Varchar(u32),
    Date,
    Time,
    Timestamp,
    Blob,
    Uuid,
    Json,
    /// A type we don't know about; passed through verbatim.
    Custom(String),
}

impl DataType {
    pub fn parse(s: &str) -> DataType {
        let lower = s.trim().to_ascii_lowercase();
        let (name, args) = match lower.find('(') {
            Some(i) if lower.ends_with(')') => {
                let args: Vec<u32> = lower[i + 1..lower.len() - 1]
                    .split(',')
                    .filter_map(|a| a.trim().parse().ok())
                    .collect();
                (lower[..i].trim().to_string(), args)
            }
            _ => (lower.clone(), vec![]),
        };
        match name.as_str() {
            "int" | "integer" | "int4" | "mediumint" => DataType::Int,
            "bigint" | "int8" => DataType::BigInt,
            "smallint" | "int2" | "tinyint" => DataType::SmallInt,
            "real" | "float" | "double" | "double precision" | "float8" | "float4" => {
                DataType::Real
            }
            "decimal" | "numeric" => DataType::Decimal(
                args.first().copied().unwrap_or(10),
                args.get(1).copied().unwrap_or(0),
            ),
            "bool" | "boolean" => DataType::Bool,
            "text" | "string" | "clob" => DataType::Text,
            "varchar" | "character varying" | "char" | "nvarchar" => {
                DataType::Varchar(args.first().copied().unwrap_or(255))
            }
            "date" => DataType::Date,
            "time" | "time without time zone" => DataType::Time,
            "timestamp" | "datetime" | "timestamptz" | "timestamp without time zone"
            | "timestamp with time zone" => DataType::Timestamp,
            "blob" | "bytes" | "bytea" | "binary" => DataType::Blob,
            "uuid" => DataType::Uuid,
            "json" | "jsonb" => DataType::Json,
            _ => DataType::Custom(s.trim().to_string()),
        }
    }

    pub fn is_integer(&self) -> bool {
        matches!(self, DataType::Int | DataType::BigInt | DataType::SmallInt)
    }
}

impl fmt::Display for DataType {
    /// Canonical DSL spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Int => write!(f, "int"),
            DataType::BigInt => write!(f, "bigint"),
            DataType::SmallInt => write!(f, "smallint"),
            DataType::Real => write!(f, "real"),
            DataType::Decimal(p, s) => write!(f, "decimal({p},{s})"),
            DataType::Bool => write!(f, "bool"),
            DataType::Text => write!(f, "text"),
            DataType::Varchar(n) => write!(f, "varchar({n})"),
            DataType::Date => write!(f, "date"),
            DataType::Time => write!(f, "time"),
            DataType::Timestamp => write!(f, "timestamp"),
            DataType::Blob => write!(f, "blob"),
            DataType::Uuid => write!(f, "uuid"),
            DataType::Json => write!(f, "json"),
            DataType::Custom(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    /// Current timestamp.
    Now,
    /// Raw SQL expression, emitted verbatim.
    Expr(String),
}

impl fmt::Display for DefaultValue {
    /// Canonical DSL spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DefaultValue::Null => write!(f, "null"),
            DefaultValue::Bool(b) => write!(f, "{b}"),
            DefaultValue::Number(n) => write!(f, "{n}"),
            DefaultValue::String(s) => write!(f, "'{}'", s.replace('\'', "''")),
            DefaultValue::Now => write!(f, "now"),
            DefaultValue::Expr(e) => write!(f, "({e})"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefAction {
    NoAction,
    Cascade,
    SetNull,
    SetDefault,
    Restrict,
}

impl RefAction {
    pub fn parse(s: &str) -> Option<RefAction> {
        match s.to_ascii_lowercase().replace(['_', '-'], " ").as_str() {
            "cascade" => Some(RefAction::Cascade),
            "set null" | "setnull" => Some(RefAction::SetNull),
            "set default" | "setdefault" => Some(RefAction::SetDefault),
            "restrict" => Some(RefAction::Restrict),
            "no action" | "noaction" => Some(RefAction::NoAction),
            _ => None,
        }
    }

    pub fn sql(&self) -> &'static str {
        match self {
            RefAction::NoAction => "NO ACTION",
            RefAction::Cascade => "CASCADE",
            RefAction::SetNull => "SET NULL",
            RefAction::SetDefault => "SET DEFAULT",
            RefAction::Restrict => "RESTRICT",
        }
    }

    /// Spelling used in the DSL (`on delete set_null`).
    pub fn dsl(&self) -> &'static str {
        match self {
            RefAction::NoAction => "no_action",
            RefAction::Cascade => "cascade",
            RefAction::SetNull => "set_null",
            RefAction::SetDefault => "set_default",
            RefAction::Restrict => "restrict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub table: String,
    pub column: String,
    pub on_delete: RefAction,
    pub on_update: RefAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub unique: bool,
    pub default: Option<DefaultValue>,
    pub references: Option<ForeignKey>,
    pub check: Option<String>,
}

impl Column {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Column {
            name: name.into(),
            data_type,
            nullable: true,
            primary_key: false,
            auto_increment: false,
            unique: false,
            default: None,
            references: None,
            check: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// Composite primary key (declared with `pk (a, b)`); empty if the key is
    /// declared inline on a single column.
    pub composite_pk: Vec<String>,
    pub indexes: Vec<Index>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// All primary key column names, whether declared inline or composite.
    pub fn primary_key(&self) -> Vec<String> {
        if !self.composite_pk.is_empty() {
            return self.composite_pk.clone();
        }
        self.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schema {
    pub tables: Vec<Table>,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// Equal apart from the order of the tables.
    pub fn same_tables(&self, other: &Schema) -> bool {
        self.tables.len() == other.tables.len() && self.tables.iter().all(|t| other.table(&t.name) == Some(t))
    }

    /// Tables ordered so that referenced tables come before the tables that
    /// reference them. Cycles are broken by falling back to declaration order.
    pub fn tables_in_dependency_order(&self) -> Vec<&Table> {
        let mut out: Vec<&Table> = Vec::new();
        let mut visiting = Vec::new();
        fn visit<'a>(
            schema: &'a Schema,
            t: &'a Table,
            out: &mut Vec<&'a Table>,
            visiting: &mut Vec<String>,
        ) {
            if out.iter().any(|o| o.name == t.name) || visiting.contains(&t.name) {
                return;
            }
            visiting.push(t.name.clone());
            for c in &t.columns {
                if let Some(fk) = &c.references
                    && fk.table != t.name
                    && let Some(dep) = schema.table(&fk.table)
                {
                    visit(schema, dep, out, visiting);
                }
            }
            visiting.pop();
            out.push(t);
        }
        for t in &self.tables {
            visit(self, t, &mut out, &mut visiting);
        }
        out
    }
}
