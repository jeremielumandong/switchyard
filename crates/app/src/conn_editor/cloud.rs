//! The connection editor's cloud services: a service picker (S3, R2, Blob Storage, App
//! Configuration, Key Vault, Secrets Manager, Parameter Store, Workers KV), the sign-in
//! method, and the fields each pair needs. Secrets go to the keychain through `password`.

use switchyard_core::store::{CloudAuth, CloudConnection, CloudService};

use super::form::{Field, FieldError, FieldSet, Values};
use super::text_input;

fn auth_id(a: CloudAuth) -> &'static str {
    match a {
        CloudAuth::AccessKey => "access-key",
        CloudAuth::AwsProfile => "aws-profile",
        CloudAuth::ApiToken => "api-token",
        CloudAuth::ConnectionString => "connection-string",
        CloudAuth::SharedKey => "shared-key",
        CloudAuth::Sas => "sas",
        CloudAuth::EntraInteractive => "entra-interactive",
        CloudAuth::EntraDeviceCode => "entra-device-code",
        CloudAuth::EntraServicePrincipal => "entra-sp",
        CloudAuth::AzureCli => "azure-cli",
    }
}

/// The sign-in method chosen in the `auth` select.
pub(super) fn chosen_auth(service: CloudService, chosen: &str) -> CloudAuth {
    service
        .auth_methods()
        .iter()
        .copied()
        .find(|a| auth_id(*a) == chosen)
        .unwrap_or(service.auth_methods()[0])
}

/// Creates the inputs and selects for `c`.
pub(super) fn init(c: &CloudConnection, f: &mut FieldSet<'_, '_, '_>) {
    f.text("name", &c.name, placeholder_name(c.service));
    f.select(
        "auth",
        c.service
            .auth_methods()
            .iter()
            .map(|a| (a.label().to_owned(), auth_id(*a).to_owned()))
            .collect(),
        auth_id(c.auth),
    );
    f.text("endpoint", &c.endpoint, endpoint_placeholder(c.service));
    f.text("region", &c.region, "us-east-1");
    f.text("user", &c.user, "");
    f.text(
        "tenant",
        c.tenant.as_deref().unwrap_or_default(),
        "contoso.onmicrosoft.com",
    );
    f.text(
        "client_id",
        c.entra_client_id.as_deref().unwrap_or_default(),
        "Azure CLI's public client",
    );
    f.text(
        "path",
        c.default_path.as_deref().unwrap_or_default(),
        match c.service {
            CloudService::AzureBlob => "Container list",
            CloudService::WorkersKv => "The first namespace",
            _ => "Bucket list",
        },
    );
    let ph = if c.secret.is_some() {
        "•••••••• (stored)"
    } else {
        ""
    };
    let i = text_input(f.window, f.cx, "", ph, true);
    f.editor.inputs.insert("password", i);
}

fn placeholder_name(s: CloudService) -> &'static str {
    match s {
        CloudService::S3 => "prod-assets",
        CloudService::R2 => "r2-media",
        CloudService::AzureBlob => "stshopprod",
        CloudService::AppConfig => "appcs-shop",
        CloudService::KeyVault => "kv-shop-prod",
        CloudService::SecretsManager => "secrets-eu",
        CloudService::ParameterStore => "params-eu",
        CloudService::WorkersKv => "workers-kv",
    }
}

fn endpoint_placeholder(s: CloudService) -> &'static str {
    match s {
        CloudService::S3 => "Empty for AWS · https://minio.local:9000",
        CloudService::SecretsManager | CloudService::ParameterStore => "Empty for AWS",
        CloudService::R2 | CloudService::WorkersKv => "0123456789abcdef0123456789abcdef",
        CloudService::AzureBlob => "stshopprod",
        CloudService::AppConfig => "appcs-shop",
        CloudService::KeyVault => "kv-shop-prod",
    }
}

/// The fields below Name, for the current sign-in method.
pub(super) fn layout(service: CloudService, auth: CloudAuth) -> Vec<Field> {
    let mut v = vec![Field::new("auth", "Sign in with")];
    let aws = matches!(
        service,
        CloudService::S3 | CloudService::SecretsManager | CloudService::ParameterStore
    );
    // Where the service is.
    match service {
        CloudService::S3 => v.push(
            Field::new("endpoint", "Custom endpoint")
                .span(4)
                .mono()
                .hint("Empty for AWS; any S3-compatible URL (MinIO, Wasabi, Backblaze B2)"),
        ),
        CloudService::SecretsManager | CloudService::ParameterStore => v.push(
            Field::new("endpoint", "Endpoint override")
                .span(4)
                .mono()
                .hint("Empty for AWS; e.g. a VPC endpoint or LocalStack"),
        ),
        CloudService::R2 | CloudService::WorkersKv => v.push(
            Field::new("endpoint", "Account ID")
                .mono()
                .hint("Cloudflare dashboard → Overview (right column), or the dashboard URL"),
        ),
        CloudService::AzureBlob if auth != CloudAuth::ConnectionString => v.push(
            Field::new("endpoint", "Storage account")
                .mono()
                .hint("The account name, or its blob endpoint URL"),
        ),
        CloudService::AppConfig if auth != CloudAuth::ConnectionString => v.push(
            Field::new("endpoint", "Store")
                .mono()
                .hint("The store name, or https://<store>.azconfig.io"),
        ),
        CloudService::KeyVault => v.push(
            Field::new("endpoint", "Vault")
                .mono()
                .hint("The vault name, or https://<vault>.vault.azure.net"),
        ),
        _ => {}
    }
    if aws {
        v.push(
            Field::new("region", "Region")
                .span(2)
                .mono()
                .hint("Empty: the profile's"),
        );
    }
    // Credentials.
    match auth {
        CloudAuth::AwsProfile => v.push(Field::new("user", "AWS profile").mono().hint(
            "A profile in ~/.aws/config (empty: default). SSO: run `aws sso login` first; \
                 roles and credential_process work too",
        )),
        CloudAuth::AccessKey => {
            v.push(
                Field::new("user", "Access key ID")
                    .span(3)
                    .mono()
                    .hint_opt((service == CloudService::R2).then_some("R2 → Manage API tokens")),
            );
            v.push(Field::password("Secret access key"));
        }
        CloudAuth::ApiToken => v.push(Field::new("password", "API token").hint(
            if service == CloudService::R2 {
                "Stored in the OS keychain · an account token with Workers R2 Storage; \
                 Switchyard derives its S3 keys"
            } else {
                "Stored in the OS keychain · needs Workers KV Storage Read (or Edit)"
            },
        )),
        CloudAuth::ConnectionString => {
            v.push(Field::new("password", "Connection string").mono().hint(
                if service == CloudService::AppConfig {
                    "Stored in the OS keychain · store → Access settings (Endpoint=…;Id=…;Secret=…)"
                } else {
                    "Stored in the OS keychain · storage account → Access keys"
                },
            ))
        }
        CloudAuth::SharedKey => v.push(
            Field::new("password", "Account key")
                .mono()
                .hint("Stored in the OS keychain · storage account → Access keys"),
        ),
        CloudAuth::Sas => v.push(
            Field::new("password", "SAS token")
                .mono()
                .hint("Stored in the OS keychain · an account or container SAS (sv=…&sig=…)"),
        ),
        CloudAuth::EntraInteractive | CloudAuth::EntraDeviceCode => {
            v.push(
                Field::new("tenant", "Tenant")
                    .span(3)
                    .mono()
                    .hint("Empty: your home directory"),
            );
            v.push(
                Field::new("client_id", "Application (client) ID")
                    .span(3)
                    .mono()
                    .hint("Optional: your own app registration"),
            );
        }
        CloudAuth::AzureCli => v.push(
            Field::new("tenant", "Tenant")
                .mono()
                .hint("Uses `az login`'s account; empty: its current tenant"),
        ),
        CloudAuth::EntraServicePrincipal => {
            v.push(Field::new("tenant", "Tenant").span(3).mono());
            v.push(Field::new("user", "Application (client) ID").span(3).mono());
            v.push(Field::new("password", "Client secret").hint("Stored in the OS keychain"));
        }
    }
    match service {
        CloudService::S3 | CloudService::R2 => v.push(
            Field::new("path", "Open at")
                .mono()
                .hint("Optional bucket and folder (my-bucket/reports); needed when the keys can't list buckets"),
        ),
        CloudService::AzureBlob => v.push(
            Field::new("path", "Open at")
                .mono()
                .hint("Optional container and folder (backups/2026); needed for a container SAS"),
        ),
        CloudService::WorkersKv => v.push(
            Field::new("path", "Namespace ID")
                .mono()
                .hint("Optional: the namespace opened first"),
        ),
        _ => {}
    }
    v
}

/// Writes the fields into `c`.
pub(super) fn apply(v: &Values<'_>, c: &mut CloudConnection) -> Result<(), FieldError> {
    let auth = chosen_auth(c.service, &v.chosen("auth"));
    c.auth = auth;
    let fields = layout(c.service, auth);
    let shown = |k: &str| fields.iter().any(|f| f.key == k);
    let text = |k: &str| if shown(k) { v.text(k) } else { String::new() };
    let opt = |k: &str| Some(text(k)).filter(|s| !s.is_empty());
    c.endpoint = text("endpoint");
    c.region = text("region");
    c.user = text("user");
    c.tenant = opt("tenant");
    c.entra_client_id = opt("client_id");
    c.default_path = opt("path").map(|p| p.trim_matches('/').to_owned());
    if c.service == CloudService::R2 && auth == CloudAuth::AccessKey && c.user.len() > 64 {
        return Err((
            Some("user"),
            "That looks like a token: R2's access key ID is 32 characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_has_credentials_or_a_sign_in() {
        for s in CloudService::ALL {
            for a in s.auth_methods() {
                let f = layout(s, *a);
                assert_eq!(chosen_auth(s, auth_id(*a)), *a);
                let has_secret = f.iter().any(|f| f.key == "password");
                assert_eq!(has_secret, a.needs_secret(), "{s:?} {a:?}");
            }
        }
    }
}
