use schemars::json_schema;
use schemars::{JsonSchema, Schema, SchemaGenerator};

pub(crate) fn schema_as_string(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("format".into(), "string".into());
    schema
}

pub(crate) fn schema_as_hex_no_prefix(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("pattern".into(), "^([0-9a-fA-F]{2})*$".into());
    schema
}

pub(crate) fn schema_as_string_array(generator: &mut SchemaGenerator) -> Schema {
    let item_schema = schema_as_string(generator);
    json_schema!({
        "type": "array",
        "items": item_schema
    })
}

pub(crate) fn schema_as_string_optional(generator: &mut SchemaGenerator) -> Schema {
    let string_schema = schema_as_string(generator);
    json_schema!({
        "anyOf": [
            string_schema,
            { "type": "null" }
        ]
    })
}

pub(crate) fn schema_as_integer(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "integer"
    })
}

pub(crate) fn schema_as_hex_bytes(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("pattern".into(), "^0x([0-9a-fA-F]{2})*$".into());
    schema
}

pub(crate) fn schema_as_hex_bytes_optional(generator: &mut SchemaGenerator) -> Schema {
    let hex_schema = schema_as_hex_bytes(generator);
    json_schema!({
        "anyOf": [
            hex_schema,
            { "type": "null" }
        ]
    })
}

pub(crate) fn schema_as_uint_hex(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("pattern".into(), "^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$".into());
    schema
}

pub(crate) fn schema_as_uint_hex_optional(generator: &mut SchemaGenerator) -> Schema {
    let hex_schema = schema_as_uint_hex(generator);
    json_schema!({
        "anyOf": [
            hex_schema,
            { "type": "null" }
        ]
    })
}
