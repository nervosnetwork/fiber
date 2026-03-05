use schemars::{
    schema::{Schema, SchemaObject},
    JsonSchema, SchemaGenerator,
};

pub(crate) fn schema_as_string(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.format = Some(String::from("string"));
    schema.into()
}

pub(crate) fn schema_as_hex_no_prefix(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.string = Some(Box::new(schemars::schema::StringValidation {
        pattern: Some("^([0-9a-fA-F]{2})*$".to_string()),
        ..Default::default()
    }));
    schema.into()
}

pub(crate) fn schema_as_string_array(generator: &mut SchemaGenerator) -> Schema {
    use schemars::schema::*;

    let item_schema = schema_as_string(generator);
    let schema = SchemaObject {
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Array))),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Single(Box::new(item_schema))),
            ..Default::default()
        })),
        ..Default::default()
    };
    schema.into()
}

pub(crate) fn schema_as_string_optional(generator: &mut SchemaGenerator) -> Schema {
    use schemars::schema::*;

    let string_schema = schema_as_string(generator);
    let null_schema: Schema = SchemaObject {
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Null))),
        ..Default::default()
    }
    .into();

    let schema = SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            any_of: Some(vec![string_schema, null_schema]),
            ..Default::default()
        })),
        ..Default::default()
    };
    schema.into()
}

pub(crate) fn schema_as_integer(_generator: &mut SchemaGenerator) -> Schema {
    use schemars::schema::*;

    let schema = SchemaObject {
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Integer))),
        ..Default::default()
    };

    schema.into()
}

pub(crate) fn schema_as_hex_bytes(generator: &mut SchemaGenerator) -> Schema {
    let mut schema: SchemaObject = String::json_schema(generator).into();
    schema.string = Some(Box::new(schemars::schema::StringValidation {
        pattern: Some("^0x([0-9a-fA-F]{2})*$".to_string()),
        ..Default::default()
    }));
    schema.into()
}

pub(crate) fn schema_as_hex_bytes_optional(generator: &mut SchemaGenerator) -> Schema {
    use schemars::schema::*;

    let hex_schema = schema_as_hex_bytes(generator);
    let null_schema: Schema = SchemaObject {
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Null))),
        ..Default::default()
    }
    .into();

    let schema = SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            any_of: Some(vec![hex_schema, null_schema]),
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

pub(crate) fn schema_as_uint_hex_optional(generator: &mut SchemaGenerator) -> Schema {
    use schemars::schema::*;

    let hex_schema = schema_as_uint_hex(generator);
    let null_schema: Schema = SchemaObject {
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Null))),
        ..Default::default()
    }
    .into();

    let schema = SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            any_of: Some(vec![hex_schema, null_schema]),
            ..Default::default()
        })),
        ..Default::default()
    };
    schema.into()
}
