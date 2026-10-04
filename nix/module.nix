self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.ldk-lnurl-server;
  serviceName = "ldk-lnurl-server";
  credentialDirectory = "/run/credentials/${serviceName}.service";
in
{
  options.services.ldk-lnurl-server = {
    enable = lib.mkEnableOption "the LNURL-pay frontend for ldk-server";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "inputs.ldk-lnurl-server.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "Package providing the ldk-lnurl-server executable.";
    };

    bindAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:3000";
      description = "Local socket address on which the HTTP service listens.";
    };

    publicUrl = lib.mkOption {
      type = lib.types.str;
      example = "https://pay.example.com/";
      description = "Externally reachable base URL used in LNURL callback responses.";
    };

    username = lib.mkOption {
      type = lib.types.str;
      default = "sats";
      description = "Single Lightning address username exposed by the service.";
    };

    paymentDescription = lib.mkOption {
      type = lib.types.str;
      default = "Pay with Lightning";
      description = "Payer-visible LNURL metadata description.";
    };

    minSendableMsat = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1000;
      description = "Minimum accepted payment in millisatoshis.";
    };

    maxSendableMsat = lib.mkOption {
      type = lib.types.ints.positive;
      default = 100000000;
      description = "Maximum accepted payment in millisatoshis.";
    };

    invoiceExpirySeconds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 3600;
      description = "Lifetime of generated BOLT11 invoices in seconds.";
    };

    ldkServerAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:3536";
      description = "ldk-server gRPC address without a URL scheme.";
    };

    tlsCertificateFile = lib.mkOption {
      type = lib.types.str;
      example = "/var/lib/ldk-server/tls.crt";
      description = ''
        Runtime path to the ldk-server TLS certificate. The file is loaded through systemd
        credentials and is not copied into the Nix store.
      '';
    };

    macaroonFile = lib.mkOption {
      type = lib.types.str;
      example = "/run/secrets/ldk-lnurl-macaroon";
      description = ''
        Runtime path to the hex-encoded ldk-server macaroon. It must grant
        `invoices:create`. The file is loaded through systemd credentials.
      '';
    };

    adminTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/run/secrets/lnurl-admin-token";
      description = ''
        Optional runtime path to an admin bearer token of at least 32 bytes. Setting this
        enables `GET /admin/invoices`. The file is loaded through systemd credentials.
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = "Tracing filter passed through RUST_LOG.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.minSendableMsat <= cfg.maxSendableMsat;
        message = "services.ldk-lnurl-server.minSendableMsat must not exceed maxSendableMsat";
      }
    ];

    systemd.services.${serviceName} = {
      description = "LNURL-pay frontend for ldk-server";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];

      environment = {
        RUST_LOG = cfg.logLevel;
        LNURL_BIND_ADDR = cfg.bindAddress;
        LNURL_PUBLIC_URL = cfg.publicUrl;
        LNURL_USERNAME = cfg.username;
        LNURL_DESCRIPTION = cfg.paymentDescription;
        LNURL_MIN_SENDABLE_MSAT = toString cfg.minSendableMsat;
        LNURL_MAX_SENDABLE_MSAT = toString cfg.maxSendableMsat;
        LNURL_INVOICE_EXPIRY_SECS = toString cfg.invoiceExpirySeconds;
        LNURL_DATABASE_PATH = "/var/lib/${serviceName}/lnurl.redb";
        LDK_SERVER_ADDRESS = cfg.ldkServerAddress;
        LDK_SERVER_TLS_CERT_FILE = "${credentialDirectory}/tls-cert";
        LDK_SERVER_MACAROON_FILE = "${credentialDirectory}/macaroon";
      }
      // lib.optionalAttrs (cfg.adminTokenFile != null) {
        LNURL_ADMIN_TOKEN_FILE = "${credentialDirectory}/admin-token";
      };

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.getExe' cfg.package "ldk-lnurl-server";
        Restart = "on-failure";
        RestartSec = "5s";

        DynamicUser = true;
        StateDirectory = serviceName;
        StateDirectoryMode = "0700";
        WorkingDirectory = "/var/lib/${serviceName}";
        UMask = "0077";

        LoadCredential = [
          "tls-cert:${cfg.tlsCertificateFile}"
          "macaroon:${cfg.macaroonFile}"
        ]
        ++ lib.optional (cfg.adminTokenFile != null) "admin-token:${cfg.adminTokenFile}";

        CapabilityBoundingSet = "";
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
      };
    };
  };
}
