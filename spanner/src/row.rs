use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use google_cloud_googleapis::spanner::v1::struct_type::Field;
use google_cloud_googleapis::spanner::v1::StructType;
use prost_types::value::Kind;
use prost_types::value::Kind::{StringValue, StructValue};
use prost_types::{value, ListValue, Value};
use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::Arc;

#[derive(Clone)]
pub struct Row {
    index: Arc<HashMap<String, usize>>,
    fields: Arc<Vec<Field>>,
    values: Vec<Value>,
}

impl Row {
    pub fn new(
        index: Arc<HashMap<String, usize>>,
        fields: Arc<Vec<Field>>,
        values: Vec<Value>,
    ) -> Row {
        Row {
            index,
            fields,
            values,
        }
    }

    pub fn column<T>(&self, column_index: usize) -> Result<T>
    where
        T: TryFromValue,
    {
        column(&self.values, &self.fields, column_index)
    }

    pub fn column_by_name<T>(&self, column_name: &str) -> Result<T>
    where
        T: TryFromValue,
    {
        self.column(index(&self.index, column_name)?)
    }
}

//don't use TryFrom trait to avoid the conflict
//https://github.com/rust-lang/rust/issues/50133
pub trait TryFromValue: Sized {
    fn try_from(value: &Value, field: &Field) -> Result<Self>;
}

pub trait TryFromStruct: Sized {
    fn try_from(s: Struct<'_>) -> Result<Self>;
}

pub struct Struct<'a> {
    index: HashMap<String, usize>,
    metadata: &'a StructType,
    values: &'a ListValue,
}

impl<'a> Struct<'a> {
    pub fn new(metadata: &'a StructType, values: &'a ListValue) -> Struct<'a> {
        let mut index = HashMap::new();
        for (i, f) in metadata.fields.iter().enumerate() {
            index.insert(f.name.clone(), i);
        }
        Struct {
            metadata,
            index,
            values,
        }
    }

    pub fn column<T>(&self, column_index: usize) -> Result<T>
    where
        T: TryFromValue,
    {
        column(&self.values.values, &self.metadata.fields, column_index)
    }

    pub fn column_by_name<T>(&self, column_name: &str) -> Result<T>
    where
        T: TryFromValue,
    {
        self.column(index(&self.index, column_name)?)
    }
}

impl TryFromValue for i64 {
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::StringValue(s) => s
                .parse()
                .map_err(|e| anyhow!("{}: i64 parse error {:?}", field.name, e)),
            v => kind_to_error(v, field),
        }
    }
}

impl TryFromValue for f64 {
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::NumberValue(s) => Ok(*s),
            v => kind_to_error(v, field),
        }
    }
}

impl TryFromValue for bool {
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::BoolValue(s) => Ok(*s),
            v => kind_to_error(v, field),
        }
    }
}

impl TryFromValue for NaiveDateTime {
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::StringValue(s) => chrono::DateTime::parse_from_rfc3339(s)
                .map(|v| v.naive_utc())
                .context(format!("{}: datetime parse error ", field.name)),
            v => kind_to_error(v, field),
        }
    }
}

impl TryFromValue for NaiveDate {
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::StringValue(s) => chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .context(format!("{}: date parse error ", field.name)),
            v => kind_to_error(v, field),
        }
    }
}

impl TryFromValue for Vec<u8> {
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::StringValue(s) => base64::decode(s).map_err(|e| e.into()),
            v => kind_to_error(v, field),
        }
    }
}

impl TryFromValue for String {
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::StringValue(s) => Ok(s.to_string()),
            v => kind_to_error(v, field),
        }
    }
}

impl<T> TryFromValue for T
where
    T: TryFromStruct,
{
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::ListValue(s) => {
                let maybe_array = match field.r#type.as_ref() {
                    None => return Err(anyhow!("field type must not be none {}", field.name)),
                    Some(tp) => tp.array_element_type.as_ref(),
                };
                let maybe_struct_type = match maybe_array {
                    None => return Err(anyhow!("array must not be none {}", field.name)),
                    Some(tp) => tp.struct_type.as_ref(),
                };
                let structured_value = match maybe_struct_type {
                    None => {
                        return Err(anyhow!(
                            "struct type in array must not be none {}",
                            field.name
                        ))
                    }
                    Some(struct_type) => Ok(Struct::new(struct_type, s)),
                };
                match structured_value {
                    Ok(v) => T::try_from(v),
                    Err(e) => Err(e),
                }
            }
            v => kind_to_error(v, field),
        }
    }
}

impl<T> TryFromValue for Option<T>
where
    T: TryFromValue,
{
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::NullValue(i) => Ok(None),
            _ => Ok(Some(T::try_from(item, field)?)),
        }
    }
}

impl<T> TryFromValue for Vec<T>
where
    T: TryFromValue,
{
    fn try_from(item: &Value, field: &Field) -> Result<Self> {
        match as_ref(item, field)? {
            Kind::ListValue(s) => s.values.iter().map(|v| T::try_from(v, field)).collect(),
            v => kind_to_error(v, field),
        }
    }
}

fn index(index: &HashMap<String, usize>, column_name: &str) -> Result<usize> {
    match index.get(column_name) {
        Some(column_index) => Ok(*column_index),
        None => Err(anyhow!("no column found: name={}", column_name)),
    }
}

fn column<T>(values: &Vec<Value>, fields: &Vec<Field>, column_index: usize) -> Result<T>
where
    T: TryFromValue,
{
    if values.len() <= column_index {
        return Err(anyhow!(
            "invalid column index: index={}, length={}",
            column_index,
            values.len()
        ));
    }
    if column_index < 0 {
        return Err(anyhow!(
            "invalid column index: index={}, length={}",
            column_index,
            values.len()
        ));
    }
    let value = &values[column_index];
    return T::try_from(value, &fields[column_index]);
}

fn as_ref<'a>(item: &'a Value, field: &'a Field) -> Result<&'a Kind> {
    item.kind
        .as_ref()
        .context(format!("{}: no kind found", field.name))
}

fn kind_to_error<'a, T>(v: &'a value::Kind, field: &'a Field) -> Result<T> {
    let actual = match v {
        Kind::StringValue(s) => "StringValue".to_string(),
        Kind::BoolValue(s) => "BoolValue".to_string(),
        Kind::NumberValue(s) => "NumberValue".to_string(),
        Kind::ListValue(s) => "ListValue".to_string(),
        Kind::StructValue(s) => "StructValue".to_string(),
        _ => "unknown".to_string(),
    };
    return Err(anyhow!("{} : Illegal Kind={}", field.name, actual));
}