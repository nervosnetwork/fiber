//! OpenRPC document generation for JSON-RPC methods.

use documented::Documented;
use jsonrpsee::core::RpcResult;
use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings};
use serde::Serialize;

/// Response to an `rpc.discover` RPC request.
pub type Response = RpcResult<ResultType>;
/// The result type for an `rpc.discover` RPC request.
pub type ResultType = OpenRpc;

pub struct RpcMethod {
    /// A description of the method.
    pub description: &'static str,
    /// A function that generates the method's parameters.
    pub params: fn(&mut Generator) -> Vec<ContentDescriptor>,
    /// A function that generates the method's result.
    pub result: fn(&mut Generator) -> ContentDescriptor,
    /// Whether the method is deprecated.
    pub deprecated: bool,
}

impl RpcMethod {
    /// Generates the OpenRPC method descriptor.
    pub fn generate(&self, generator: &mut Generator, name: &'static str) -> Method {
        self.generate_with_tag(generator, name, None)
    }

    /// Generates the OpenRPC method descriptor with an optional tag.
    pub fn generate_with_tag(
        &self,
        generator: &mut Generator,
        name: &'static str,
        tag: Option<&str>,
    ) -> Method {
        let description = self.description.trim();

        Method {
            name,
            summary: description
                .split_once('\n')
                .map(|(summary, _)| summary)
                .unwrap_or(description),
            description,
            params: (self.params)(generator),
            result: (self.result)(generator),
            deprecated: self.deprecated,
            tags: tag
                .map(|t| {
                    vec![Tag {
                        name: t.to_string(),
                    }]
                })
                .unwrap_or_default(),
        }
    }
}

/// An OpenRPC document generator.
pub struct Generator {
    inner: SchemaGenerator,
}

impl Generator {
    /// Creates a new OpenRPC document generator.
    pub fn new() -> Self {
        Self {
            inner: SchemaSettings::draft07()
                .with(|s| {
                    s.definitions_path = "#/components/schemas/".into();
                })
                .into_generator(),
        }
    }

    /// Constructs the descriptor for a JSON-RPC method parameter.
    pub fn param<T: JsonSchema>(
        &mut self,
        name: &'static str,
        description: &'static str,
        required: bool,
    ) -> ContentDescriptor {
        ContentDescriptor {
            name,
            summary: description
                .split_once('\n')
                .map(|(summary, _)| summary)
                .unwrap_or(description),
            description,
            required,
            schema: self.inner.subschema_for::<T>(),
            deprecated: false,
        }
    }

    /// Constructs the descriptor for a JSON-RPC method's result type.
    pub fn result<T: Documented + JsonSchema>(&mut self, name: &'static str) -> ContentDescriptor {
        ContentDescriptor {
            name,
            summary: T::DOCS
                .split_once('\n')
                .map(|(summary, _)| summary)
                .unwrap_or(T::DOCS),
            description: T::DOCS,
            required: false,
            schema: self.inner.subschema_for::<T>(),
            deprecated: false,
        }
    }

    /// Constructs the descriptor for a JSON-RPC method's result type.
    ///
    /// Unlike [`result`](Self::result), this method does not require `T` to implement
    /// `Documented`, accepting a description parameter directly instead.
    pub fn result_schema<T: JsonSchema>(
        &mut self,
        name: &'static str,
        description: &'static str,
    ) -> ContentDescriptor {
        ContentDescriptor {
            name,
            summary: description
                .split_once('\n')
                .map(|(summary, _)| summary)
                .unwrap_or(description),
            description,
            required: false,
            schema: self.inner.subschema_for::<T>(),
            deprecated: false,
        }
    }

    /// Consumes the generator and produces the OpenRPC components.
    pub fn into_components(mut self) -> Components {
        Components {
            schemas: self.inner.take_definitions(true),
        }
    }
}

/// An OpenRPC document.
#[derive(Clone, Debug, Serialize, Documented)]
pub struct OpenRpc {
    /// The OpenRPC specification version.
    pub openrpc: &'static str,
    /// Information about the API.
    pub info: Info,
    /// The available JSON-RPC methods.
    pub methods: Vec<Method>,
    /// The components (schemas) used in the document.
    pub components: Components,
}

impl JsonSchema for OpenRpc {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("OpenRPC Schema")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        Schema::new_ref(
            "https://raw.githubusercontent.com/open-rpc/meta-schema/master/schema.json".into(),
        )
    }
}

/// Information about the API.
#[derive(Clone, Debug, Serialize)]
pub struct Info {
    /// The title of the API.
    pub title: &'static str,
    /// A description of the API.
    pub description: &'static str,
    /// The version of the API.
    pub version: &'static str,
}

/// A JSON-RPC method.
#[derive(Clone, Debug, Serialize)]
pub struct Method {
    name: &'static str,
    summary: &'static str,
    description: &'static str,
    params: Vec<ContentDescriptor>,
    result: ContentDescriptor,
    #[serde(skip_serializing_if = "is_false")]
    deprecated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<Tag>,
}

/// A tag for a JSON-RPC method.
#[derive(Clone, Debug, Serialize)]
pub struct Tag {
    name: String,
}

/// A descriptor for a JSON-RPC method's parameter or result.
#[derive(Clone, Debug, Serialize)]
pub struct ContentDescriptor {
    name: &'static str,
    summary: &'static str,
    description: &'static str,
    #[serde(skip_serializing_if = "is_false")]
    required: bool,
    schema: Schema,
    #[serde(skip_serializing_if = "is_false")]
    deprecated: bool,
}

/// The components (schemas) used in the OpenRPC document.
#[derive(Clone, Debug, Serialize)]
pub struct Components {
    schemas: serde_json::Map<String, serde_json::Value>,
}

fn is_false(b: &bool) -> bool {
    !b
}
