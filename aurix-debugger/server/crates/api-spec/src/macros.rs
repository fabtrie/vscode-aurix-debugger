/// Declares operations of the API:
///
/// ```text
/// api! {
///     trait DebuggerApi; table OPS; dispatch dispatch; scope Session;
///
///     /// Documentation (MCP tool description, OpenAPI description, Python docstring).
///     op name [tag]: METHOD "/route/{param}" (RequestType) -> ResponseType;
/// }
/// ```
///
/// Generates the trait (one method per operation), the [`OpMeta`](crate::OpMeta) table and the
/// dispatch function. Used twice: for the session operations a worker implements and for the
/// hub's own operations.
macro_rules! api {
    (
        trait $trait:ident; table $table:ident; dispatch $dispatch:ident; scope $scope:ident;
        $(
            $(#[doc = $doc:literal])*
            op $name:ident [$tag:ident] : $method:ident $path:literal ($req:ty) -> $resp:ty;
        )*
    ) => {
        /// The operations, implemented by the server.
        pub trait $trait: Send + Sync {
            $(
                $(#[doc = $doc])*
                fn $name(&self, req: $req) -> Result<$resp, $crate::ApiError>;
            )*
        }

        /// All operations in declaration order.
        pub static $table: &[$crate::OpMeta] = &[$(
            $crate::OpMeta {
                name: stringify!($name),
                tag: stringify!($tag),
                scope: $crate::Scope::$scope,
                method: $crate::Method::$method,
                path: $path,
                doc: concat!($($doc, "\n"),*),
                request: |g| g.subschema_for::<$req>(),
                response: |g| g.subschema_for::<$resp>(),
            },
        )*];

        /// Runs operation `op` with its arguments as JSON object (fields of the request type).
        pub fn $dispatch(api: &dyn $trait, op: &str, args: serde_json::Value) -> Result<serde_json::Value, $crate::ApiError> {
            let args = if args.is_null() { serde_json::Value::Object(Default::default()) } else { args };
            match op {
                $(
                    stringify!($name) => {
                        let req: $req = serde_json::from_value(args)
                            .map_err(|e| $crate::ApiError::bad_request(format!("{}: {e}", stringify!($name))))?;
                        let resp = api.$name(req)?;
                        Ok(serde_json::to_value(resp).expect("responses serialize"))
                    }
                )*
                _ => Err($crate::ApiError::not_found(format!("unknown operation '{op}'"))),
            }
        }
    };
}
