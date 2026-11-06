//! Service model access (botocore `model.py`).
//!
//! The model is kept as raw JSON so every trait in `service-2.json` stays
//! reachable without mirroring the full schema in Rust types. A [`Shape`] is a
//! zero-copy view: a shape definition plus the traits of the member reference
//! that pointed at it. Member traits take precedence, exactly like
//! `ShapeResolver.resolve_shape_ref` merging them over the definition.

use serde_json::{Map, Value as J};
use std::fmt;

#[derive(Debug)]
pub enum ModelError {
    UnknownShape(String),
    UnknownOperation(String),
    Invalid(String),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::UnknownShape(s) => write!(f, "unknown shape: {s}"),
            ModelError::UnknownOperation(s) => write!(f, "unknown operation: {s}"),
            ModelError::Invalid(s) => write!(f, "invalid model: {s}"),
        }
    }
}

impl std::error::Error for ModelError {}

static EMPTY: std::sync::LazyLock<Map<String, J>> = std::sync::LazyLock::new(Map::new);

/// Protocols in the order botocore prefers them when a model lists several
/// (`utils.PRIORITY_ORDERED_SUPPORTED_PROTOCOLS`).
pub const PRIORITY_ORDERED_PROTOCOLS: &[&str] = &[
    "json",
    "rest-json",
    "rest-xml",
    "smithy-rpc-v2-cbor",
    "query",
    "ec2",
];

#[derive(Debug, Clone)]
pub struct ServiceModel {
    raw: J,
    /// CLI-level service name (directory name under `data/`).
    pub service_name: String,
}

impl ServiceModel {
    pub fn from_json(service_name: &str, raw: J) -> Result<Self, ModelError> {
        if !raw.is_object() {
            return Err(ModelError::Invalid("model root is not an object".into()));
        }
        Ok(ServiceModel {
            raw,
            service_name: service_name.to_string(),
        })
    }

    pub fn metadata(&self) -> &Map<String, J> {
        self.raw
            .get("metadata")
            .and_then(J::as_object)
            .unwrap_or(&EMPTY)
    }

    pub fn meta_str(&self, key: &str) -> Option<&str> {
        self.metadata().get(key).and_then(J::as_str)
    }

    pub fn api_version(&self) -> &str {
        self.meta_str("apiVersion").unwrap_or("")
    }

    pub fn endpoint_prefix(&self) -> &str {
        self.meta_str("endpointPrefix")
            .unwrap_or(&self.service_name)
    }

    pub fn signing_name(&self) -> &str {
        self.meta_str("signingName")
            .unwrap_or_else(|| self.endpoint_prefix())
    }

    pub fn service_id(&self) -> &str {
        self.meta_str("serviceId").unwrap_or("")
    }

    pub fn is_query_compatible(&self) -> bool {
        self.metadata().contains_key("awsQueryCompatible")
    }

    /// `ServiceModel.resolved_protocol`.
    pub fn protocol(&self) -> &str {
        if let Some(list) = self.metadata().get("protocols").and_then(J::as_array) {
            for p in PRIORITY_ORDERED_PROTOCOLS {
                if list.iter().any(|v| v.as_str() == Some(p)) {
                    return p;
                }
            }
        }
        self.meta_str("protocol").unwrap_or("")
    }

    pub fn raw(&self) -> &J {
        &self.raw
    }

    fn shapes(&self) -> &Map<String, J> {
        self.raw
            .get("shapes")
            .and_then(J::as_object)
            .unwrap_or(&EMPTY)
    }

    pub fn operation_names(&self) -> impl Iterator<Item = &str> {
        self.raw
            .get("operations")
            .and_then(J::as_object)
            .into_iter()
            .flat_map(|m| m.keys().map(String::as_str))
    }

    pub fn operation(&self, name: &str) -> Result<Operation<'_>, ModelError> {
        let def = self
            .raw
            .get("operations")
            .and_then(|o| o.get(name))
            .and_then(J::as_object)
            .ok_or_else(|| ModelError::UnknownOperation(name.to_string()))?;
        Ok(Operation {
            model: self,
            name: name.to_string(),
            def,
        })
    }

    pub fn shape(&self, name: &str) -> Result<Shape<'_>, ModelError> {
        let def = self
            .shapes()
            .get(name)
            .and_then(J::as_object)
            .ok_or_else(|| ModelError::UnknownShape(name.to_string()))?;
        Ok(Shape {
            model: self,
            name: name.to_string(),
            def,
            traits: &EMPTY,
        })
    }

    /// Resolve `{"shape": "Name", ...member traits}`.
    pub fn resolve_ref<'a>(&'a self, r: &'a J) -> Result<Shape<'a>, ModelError> {
        let traits = r
            .as_object()
            .ok_or_else(|| ModelError::Invalid("shape ref".into()))?;
        let name = traits
            .get("shape")
            .and_then(J::as_str)
            .ok_or_else(|| ModelError::Invalid("shape ref without shape".into()))?;
        let mut s = self.shape(name)?;
        s.traits = traits;
        Ok(s)
    }

    /// Find the error shape whose code matches (`shape_for_error_code`).
    pub fn shape_for_error_code(&self, code: &str) -> Option<Shape<'_>> {
        for (name, def) in self.shapes() {
            if def.get("exception").and_then(J::as_bool) != Some(true) {
                continue;
            }
            let declared = def
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(J::as_str);
            if declared.unwrap_or(name) == code {
                return self.shape(name).ok();
            }
        }
        None
    }
}

#[derive(Clone)]
pub struct Operation<'a> {
    pub model: &'a ServiceModel,
    pub name: String,
    pub def: &'a Map<String, J>,
}

impl<'a> Operation<'a> {
    /// Name sent on the wire (`OperationModel.wire_name`).
    pub fn wire_name(&self) -> &str {
        self.def
            .get("name")
            .and_then(J::as_str)
            .unwrap_or(&self.name)
    }

    pub fn http(&self) -> &'a Map<String, J> {
        self.def
            .get("http")
            .and_then(J::as_object)
            .unwrap_or(&EMPTY)
    }

    pub fn http_method(&self) -> &'a str {
        self.http()
            .get("method")
            .and_then(J::as_str)
            .unwrap_or("POST")
    }

    pub fn request_uri(&self) -> &'a str {
        self.http()
            .get("requestUri")
            .and_then(J::as_str)
            .unwrap_or("/")
    }

    pub fn input(&self) -> Option<Shape<'a>> {
        self.def
            .get("input")
            .and_then(|r| self.model.resolve_ref(r).ok())
    }

    pub fn output(&self) -> Option<Shape<'a>> {
        self.def
            .get("output")
            .and_then(|r| self.model.resolve_ref(r).ok())
    }

    /// The input payload is a blob (`OperationModel.has_streaming_input`).
    pub fn has_streaming_input(&self) -> bool {
        self.input().is_some_and(|s| {
            s.payload()
                .and_then(|p| s.member_named(p))
                .is_some_and(|m| m.type_name() == "blob")
        })
    }

    /// Output payload member that is a blob (`get_streaming_output`), the
    /// target of the CLI `outfile` argument.
    pub fn streaming_output_member(&self) -> Option<&'a str> {
        let s = self.output()?;
        let p = s.payload()?;
        (s.member_named(p)?.type_name() == "blob").then_some(p)
    }

    pub fn host_prefix(&self) -> Option<&'a str> {
        self.def
            .get("endpoint")
            .and_then(|e| e.get("hostPrefix"))
            .and_then(J::as_str)
    }

    pub fn get(&self, key: &str) -> Option<&'a J> {
        self.def.get(key)
    }

    pub fn documentation(&self) -> &'a str {
        self.def
            .get("documentation")
            .and_then(J::as_str)
            .unwrap_or("")
    }

    pub fn deprecated(&self) -> bool {
        self.def
            .get("deprecated")
            .and_then(J::as_bool)
            .unwrap_or(false)
    }
}

#[derive(Clone)]
pub struct Shape<'a> {
    pub model: &'a ServiceModel,
    pub name: String,
    def: &'a Map<String, J>,
    traits: &'a Map<String, J>,
}

impl fmt::Debug for Shape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Shape({}:{})", self.name, self.type_name())
    }
}

impl<'a> Shape<'a> {
    /// Look up a trait: member reference first, then the shape definition.
    pub fn get(&self, key: &str) -> Option<&'a J> {
        self.traits.get(key).or_else(|| self.def.get(key))
    }

    pub fn str_trait(&self, key: &str) -> Option<&'a str> {
        self.get(key).and_then(J::as_str)
    }

    pub fn bool_trait(&self, key: &str) -> bool {
        self.get(key).and_then(J::as_bool).unwrap_or(false)
    }

    pub fn type_name(&self) -> &'a str {
        self.def.get("type").and_then(J::as_str).unwrap_or("")
    }

    /// Serialized name (`locationName`), botocore's `serialization['name']`.
    pub fn ser_name(&self) -> Option<&'a str> {
        self.str_trait("locationName")
    }

    pub fn location(&self) -> Option<&'a str> {
        self.str_trait("location")
    }

    pub fn flattened(&self) -> bool {
        self.bool_trait("flattened")
    }

    pub fn timestamp_format(&self) -> Option<&'a str> {
        self.str_trait("timestampFormat")
    }

    pub fn is_document(&self) -> bool {
        self.bool_trait("document")
    }

    pub fn is_streaming(&self) -> bool {
        self.bool_trait("streaming")
    }

    pub fn is_jsonvalue(&self) -> bool {
        self.bool_trait("jsonvalue")
    }

    pub fn payload(&self) -> Option<&'a str> {
        self.def.get("payload").and_then(J::as_str)
    }

    pub fn required(&self) -> Vec<&'a str> {
        self.def
            .get("required")
            .and_then(J::as_array)
            .map(|a| a.iter().filter_map(J::as_str).collect())
            .unwrap_or_default()
    }

    pub fn enum_values(&self) -> Vec<&'a str> {
        self.def
            .get("enum")
            .and_then(J::as_array)
            .map(|a| a.iter().filter_map(J::as_str).collect())
            .unwrap_or_default()
    }

    pub fn documentation(&self) -> &'a str {
        self.str_trait("documentation").unwrap_or("")
    }

    /// Structure members in model order.
    pub fn members(&self) -> Vec<(&'a str, Shape<'a>)> {
        let Some(m) = self.def.get("members").and_then(J::as_object) else {
            return Vec::new();
        };
        m.iter()
            .filter_map(|(k, r)| self.model.resolve_ref(r).ok().map(|s| (k.as_str(), s)))
            .collect()
    }

    pub fn member_named(&self, name: &str) -> Option<Shape<'a>> {
        let r = self.def.get("members")?.get(name)?;
        self.model.resolve_ref(r).ok()
    }

    /// List element shape.
    pub fn member(&self) -> Option<Shape<'a>> {
        self.def
            .get("member")
            .and_then(|r| self.model.resolve_ref(r).ok())
    }

    pub fn key(&self) -> Option<Shape<'a>> {
        self.def
            .get("key")
            .and_then(|r| self.model.resolve_ref(r).ok())
    }

    pub fn value(&self) -> Option<Shape<'a>> {
        self.def
            .get("value")
            .and_then(|r| self.model.resolve_ref(r).ok())
    }

    /// Name of the event-stream member, if any.
    pub fn event_stream_member(&self) -> Option<&'a str> {
        let m = self.def.get("members")?.as_object()?;
        m.iter().find_map(|(k, r)| {
            let s = self.model.resolve_ref(r).ok()?;
            s.bool_trait("eventstream").then_some(k.as_str())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> ServiceModel {
        ServiceModel::from_json(
            "test",
            json!({
                "metadata": {"protocol": "query", "protocols": ["json", "query"], "apiVersion": "2020-01-01", "endpointPrefix": "tst"},
                "operations": {"Op": {"name": "Op", "http": {"method": "POST", "requestUri": "/"}, "input": {"shape": "In"}}},
                "shapes": {
                    "In": {"type": "structure", "members": {"B": {"shape": "S", "locationName": "bee"}, "A": {"shape": "S"}}},
                    "S": {"type": "string", "locationName": "ignored"}
                }
            }),
        )
        .unwrap()
    }

    #[test]
    fn member_traits_override_definition() {
        let m = model();
        let input = m.operation("Op").unwrap().input().unwrap();
        let members = input.members();
        assert_eq!(members[0].0, "B", "member order is preserved");
        assert_eq!(members[0].1.ser_name(), Some("bee"));
        assert_eq!(members[1].1.ser_name(), Some("ignored"));
    }

    #[test]
    fn protocol_priority() {
        assert_eq!(model().protocol(), "json");
        assert_eq!(model().signing_name(), "tst");
    }
}
