# ldk-lnurl-server

A deliberately small [LNURL-pay](https://github.com/lnurl/luds/blob/luds/06.md) HTTP frontend for
[ldk-server](https://github.com/lightningdevkit/ldk-server). It exposes one Lightning address
name and asks ldk-server to create a fixed-amount BOLT11 invoice for each valid callback.

The service keeps a small embedded [redb](https://docs.rs/redb/) database of invoices it issued.
ldk-server remains the source of truth for payment status and balances.

The canonical repository is
[`git.cashu.dev/thesimplekid/ldk-lnurl-server`](https://git.cashu.dev/thesimplekid/ldk-lnurl-server).
It is mirrored to
[`github.com/thesimplekid/ldk-lnurl-server`](https://github.com/thesimplekid/ldk-lnurl-server).

## Run it

1. Start ldk-server.

2. Enter the reproducible development shell, then create a restricted macaroon file:

   ```console
   nix develop path:.
   umask 077
   ldk-server-cli create-macaroon lnurl --preset invoice | jq -er .token > lnurl.macaroon
   ```

3. Configure the connection and start the service:

   ```console
   export LDK_SERVER_ADDRESS=127.0.0.1:3536
   export LDK_SERVER_TLS_CERT_FILE=/path/to/ldk-server/tls.crt
   export LDK_SERVER_MACAROON_FILE=$PWD/lnurl.macaroon
   export LNURL_PUBLIC_URL=https://pay.example.com/
   export LNURL_USERNAME=alice
   openssl rand -hex 32 > admin.token
   export LNURL_ADMIN_TOKEN_FILE=$PWD/admin.token
   just run
   ```

4. Point `alice@pay.example.com` clients at:

   ```text
   https://pay.example.com/.well-known/lnurlp/alice
   ```

`LNURL_PUBLIC_URL` must be the externally reachable URL. Put a TLS-terminating reverse proxy in
front of the service for public deployment; the Rust process listens on `127.0.0.1:3000` by
default.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `LDK_SERVER_ADDRESS` | `127.0.0.1:3536` | ldk-server gRPC address, without a URL scheme |
| `LDK_SERVER_TLS_CERT_FILE` | required | ldk-server TLS certificate in PEM format |
| `LDK_SERVER_MACAROON_FILE` | required | hex-encoded macaroon with `invoices:create` permission |
| `LNURL_BIND_ADDR` | `127.0.0.1:3000` | local HTTP listen address |
| `LNURL_PUBLIC_URL` | `http://127.0.0.1:3000/` | public base URL used in callbacks |
| `LNURL_USERNAME` | `sats` | the single exposed Lightning address name |
| `LNURL_DESCRIPTION` | `Pay with Lightning` | payer-visible metadata and invoice description hash source |
| `LNURL_MIN_SENDABLE_MSAT` | `1000` | minimum amount in millisatoshis |
| `LNURL_MAX_SENDABLE_MSAT` | `100000000` | maximum amount in millisatoshis |
| `LNURL_INVOICE_EXPIRY_SECS` | `3600` | BOLT11 invoice expiry |
| `LNURL_DATABASE_PATH` | `lnurl.redb` | embedded database containing LNURL-issued invoices |
| `LNURL_ADMIN_TOKEN_FILE` | unset | enables the admin API using the token in this file |

Secrets are accepted only through files so they do not need to appear in process arguments or
the environment. Keep the LNURL service and ldk-server clocks synchronized because ldk-server
binds macaroon credentials to each request and time.

The directory containing `LNURL_DATABASE_PATH` must already exist and be writable. Run only one
service process against a database file; redb intentionally locks an open database.

## Invoice history

When `LNURL_ADMIN_TOKEN_FILE` is configured, the service exposes the invoices it issued:

```console
curl --fail --silent \
  -H "Authorization: Bearer $(cat admin.token)" \
  http://127.0.0.1:3000/admin/invoices | jq
```

Each record contains the payment hash, BOLT11 invoice, amount, LNURL username, issue time, and
expiry time. Match its `payment_hash` against `ldk-server-cli list-payments` to distinguish
payments resulting from this LNURL service from invoices created elsewhere. Unpaid and expired
LNURL invoices remain in redb, even though they do not appear in ldk-server's payment history.

The admin endpoint returns `404` when it is disabled and requires a bearer token when enabled.
Keep it behind HTTPS or restricted to a trusted network; its response contains payable invoices.

## NixOS module

The flake exports `nixosModules.default`. The following example uses
[sops-nix](https://github.com/Mic92/sops-nix) for runtime secrets and nginx for public HTTPS.
For a checkout already present on the server, a `path:/absolute/path/to/ldk-lnurl-server` input
can be used instead. The GitHub mirror can also be selected with
`github:thesimplekid/ldk-lnurl-server`.

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    ldk-lnurl-server.url =
      "git+https://git.cashu.dev/thesimplekid/ldk-lnurl-server";

    sops-nix.url = "github:Mic92/sops-nix";
    sops-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, ldk-lnurl-server, sops-nix, ... }: {
    nixosConfigurations.server = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ldk-lnurl-server.nixosModules.default
        sops-nix.nixosModules.sops
        ({ config, ... }: {
          # Encrypt this file with sops; never commit plaintext credentials.
          sops.defaultSopsFile = ./secrets.yaml;
          sops.secrets.ldk-lnurl-macaroon = { };
          sops.secrets.lnurl-admin-token = { };

          services.ldk-lnurl-server = {
            enable = true;
            publicUrl = "https://pay.example.com/";
            username = "alice";
            tlsCertificateFile = "/var/lib/ldk-server/tls.crt";
            macaroonFile = config.sops.secrets.ldk-lnurl-macaroon.path;
            adminTokenFile = config.sops.secrets.lnurl-admin-token.path;
          };

          security.acme = {
            acceptTerms = true;
            defaults.email = "admin@example.com";
          };

          services.nginx = {
            enable = true;
            recommendedProxySettings = true;
            recommendedTlsSettings = true;
            virtualHosts."pay.example.com" = {
              enableACME = true;
              forceSSL = true;

              # Only expose the two public LNURL route families. In particular,
              # /admin/invoices remains reachable only from the server itself.
              locations."/.well-known/lnurlp/".proxyPass =
                "http://127.0.0.1:3000";
              locations."/api/v1/lnurl/callback/".proxyPass =
                "http://127.0.0.1:3000";
            };
          };

          networking.firewall.allowedTCPPorts = [ 80 443 ];
          system.stateVersion = "25.11"; # Use the release of the original install.
        })
      ];
    };
  };
}
```

Before switching to this configuration:

1. Point the domain's `A`/`AAAA` records at the server and allow inbound TCP ports 80 and 443.
2. Create a restricted ldk-server macaroon with `invoices:create` permission:

   ```console
   umask 077
   ldk-server-cli create-macaroon lnurl --preset invoice | jq -er .token > lnurl.macaroon
   openssl rand -hex 32 > lnurl-admin-token
   ```

3. Add the contents of those two files to `secrets.yaml` under `ldk-lnurl-macaroon` and
   `lnurl-admin-token`, then encrypt it according to the sops-nix documentation. The macaroon
   value is the hex string printed by ldk-server; the admin token is an independent secret.
4. Deploy with `sudo nixos-rebuild switch --flake .#server`.

The module runs under a dynamic systemd user and stores redb state in
`/var/lib/ldk-lnurl-server/lnurl.redb`. TLS, macaroon, and optional admin-token files are supplied
with systemd credentials, so their contents are not copied into the Nix store. Use sops-nix,
agenix, or another secret provisioner to create the runtime secret paths.

The HTTP listener remains on `127.0.0.1:3000` by default. If ldk-server is on another host, set
`services.ldk-lnurl-server.ldkServerAddress` and provide the TLS certificate expected for that
connection. If it is a local NixOS service, it can additionally be ordered after that unit:

```nix
systemd.services.ldk-lnurl-server = {
  after = [ "ldk-server.service" ];
  wants = [ "ldk-server.service" ];
};
```

### Verify and operate the NixOS service

After deployment, check the unit, local health endpoint, public LNURL endpoint, and logs:

```console
systemctl status ldk-lnurl-server
curl --fail http://127.0.0.1:3000/healthz
curl --fail https://pay.example.com/.well-known/lnurlp/alice | jq
journalctl -u ldk-lnurl-server -f
```

The admin endpoint is intentionally not proxied by the nginx example. Query it locally using the
same token stored by the secret provisioner:

```console
curl --fail --silent \
  -H "Authorization: Bearer $(sudo cat /run/secrets/lnurl-admin-token)" \
  http://127.0.0.1:3000/admin/invoices | jq
```

This lists invoices issued through LNURL, including unpaid invoices. To determine what was
actually received, run `ldk-server-cli list-payments` and select successful inbound payments whose
BOLT11 hash matches an LNURL record's `payment_hash`.

Back up `/var/lib/ldk-lnurl-server/lnurl.redb` along with the ldk-server data. Stop
`ldk-lnurl-server.service` while taking a plain filesystem copy so that the redb file is not being
modified. The database contains attribution history, while ldk-server remains the source of truth
for whether a payment succeeded.

## Development

The Rust version is pinned in `rust-toolchain.toml` and consumed by the Nix flake. Common checks
are available through Just:

```console
just quick-check
just final-check
```

Build the reproducible package with `nix build path:.`. The resulting executable is
`result/bin/ldk-lnurl-server`.
