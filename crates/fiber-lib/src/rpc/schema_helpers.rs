use schemars::{JsonSchema, Schema, SchemaGenerator};

pub(crate) fn schema_as_string(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("format".into(), "string".into());
    schema
}
