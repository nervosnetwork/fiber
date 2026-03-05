use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use schemars::schema::SchemaObject;
use schemars::JsonSchema;

pub fn schema_as_string(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.format = Some(String::from("string"));
    schema.into()
}

pub fn schema_as_hex_bytes(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.string = Some(Box::new(schemars::schema::StringValidation {
        pattern: Some("^0x([0-9a-fA-F]{2})*$".to_string()),
        ..Default::default()
    }));
    schema.into()
}

pub fn schema_as_hex_no_prefix(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.string = Some(Box::new(schemars::schema::StringValidation {
        pattern: Some("^([0-9a-fA-F]{2})*$".to_string()),
        ..Default::default()
    }));
    schema.into()
}

pub fn schema_as_any(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.format = Some(String::from("any"));
    schema.into()
}

pub fn schema_as_byte_array(_generator: &mut SchemaGenerator) -> Schema {
    use schemars::schema::*;

    let item_schema = SchemaObject {
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Integer))),
        number: Some(Box::new(NumberValidation {
            minimum: Some(0.0),
            maximum: Some(255.0),
            ..Default::default()
        })),
        ..Default::default()
    };

    let schema = SchemaObject {
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Array))),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Single(Box::new(item_schema.into()))),
            ..Default::default()
        })),
        ..Default::default()
    };

    schema.into()
}

pub(crate) fn schema_as_uint_hex(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.string = Some(Box::new(schemars::schema::StringValidation {
        pattern: Some("^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$".to_string()),
        ..Default::default()
    }));
    schema.into()
}
