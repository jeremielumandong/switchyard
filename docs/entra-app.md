# Switchyard's Microsoft Entra app registration

Azure SQL sign-in with Microsoft Entra ID (browser with MFA, device code, password) goes
through one **multi-tenant public client** app registration. People from any organization
sign in with their own work account; they do not join the tenant that holds the
registration. Service principals use their own app id instead.

## Create it (once, by a project maintainer)

1. In any Entra tenant (a free one created for the project is fine), open
   **Microsoft Entra admin center → App registrations → New registration**.
   - Name: `Switchyard`
   - Supported account types: **Accounts in any organizational directory (multitenant)**
   - Redirect URI: platform **Public client/native (mobile & desktop)**, value
     `http://localhost`. (Entra accepts any port on `http://localhost` for this platform;
     Switchyard picks a free port per sign-in.)
2. **Authentication**: set **Allow public client flows** to **Yes** (needed for device code
   and password sign-in). Add no client secret: the app is public and ships in the binary.
3. **API permissions → Add a permission → APIs my organization uses → Azure SQL Database →
   Delegated → `user_impersonation`**. Keep Microsoft Graph `User.Read` (default) or remove
   it; Switchyard does not call Graph.
4. Optional but recommended: **Branding & properties** (logo, publisher domain, terms and
   privacy links) and **publisher verification**, so the consent screen shows a verified
   publisher. Some organizations only allow user consent to verified publishers.
5. Copy the **Application (client) ID** and build with it:

   ```bash
   SWITCHYARD_ENTRA_CLIENT_ID=<application id> cargo build --release -p switchyard-app
   ```

   Release CI reads it from a repository variable of the same name. It is not a secret.

## What users see

- First sign-in from an organization: Microsoft's consent screen ("Switchyard wants to
  access Azure SQL Database as you"). If the organization blocks user consent, an admin
  approves once for everyone: `https://login.microsoftonline.com/<tenant>/adminconsent?client_id=<application id>`.
- The database still decides access: the account needs a database user, e.g.
  `CREATE USER [name@company.com] FROM EXTERNAL PROVIDER;`.
- Personal Microsoft accounts (outlook.com) cannot log in to Azure SQL, so the app only
  targets work and school accounts.

## Organizations that require their own registration

They create a registration as above in their own tenant (single-tenant is fine) and put its
application id in the connection editor (**Application (client) id**), with their tenant.
