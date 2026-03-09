use schemars::json_schema;
use schemars::JsonSchema;
use schemars::Schema;
use schemars::SchemaGenerator;

pub fn schema_as_string(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("format".into(), "string".into());
    schema
}

pub fn schema_as_hex_bytes(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("pattern".into(), "^0x([0-9a-fA-F]{2})*$".into());
    schema
}

pub fn schema_as_hex_no_prefix(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("pattern".into(), "^([0-9a-fA-F]{2})*$".into());
    schema
}

pub fn schema_as_any(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("format".into(), "any".into());
    schema
}

pub fn schema_as_byte_array(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "array",
        "items": {
            "type": "integer",
            "minimum": 0,
            "maximum": 255
        }
    })
}

pub(crate) fn schema_as_uint_hex(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = String::json_schema(generator);
    schema.insert("pattern".into(), "^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$".into());
    schema
}
