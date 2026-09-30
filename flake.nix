{
  description = "Eva kernel: the runtime she lives in, plus its protocol, SDK and services";

  inputs = {
    eva.url = "git+https://git.desu.church/VienDesu/eva.git";
    flake-utils.follows = "eva/flake-utils";
  };

  outputs = { self, eva, flake-utils, ... }:
    let
      # Ревизия в бинарь: .git в песочницу сборки не едет, а знать, что
      # именно крутится, нужно и Еве (промпт), и метрикам.
      commit = self.shortRev or self.dirtyShortRev or "dev";

      # Один воркспейс, несколько бинарей: crane собирает весь граф один раз
      # (общий cargoArtifacts), `--package` выбирает, что установить. `env`
      # у mkPackage сливается в аргументы crane, поэтому cargoExtraArgs
      # передаётся через него.
      service = pkgs: pname: eva.lib.mkPackage {
        inherit pkgs pname;
        src = self;
        env = {
          cargoExtraArgs = "--locked --package ${pname}";
          EVA_COMMIT = commit;
        };
      };

      pname = "eva-kernel";
      credentialsDir = "/run/credentials/${pname}.service";

      # Поля-секреты конфига (`SecretString`): `<section>.<name>.<field>`.
      secretFields = [
        { section = [ "llm" "providers" ]; field = "api_key"; }
        { section = [ "modules" "tokens" ]; field = "token"; }
        { section = [ "modules" "services" ]; field = "key"; }
        { section = [ "hands" "endpoints" ]; field = "key"; }
      ];

      # Секреты settings в форме `form` (`plaintext` | `file`): путь ключа → значение.
      secretsOf = lib: form: settings: lib.concatMap
        ({ section, field }:
          lib.concatLists (lib.mapAttrsToList
            (name: entry:
              let secret = if builtins.isAttrs entry then entry.${field} or null else null;
              in lib.optional (builtins.isAttrs secret && secret ? ${form}) {
                key = lib.concatStringsSep "." (section ++ [ name field ]);
                value = secret.${form};
              })
            (lib.attrByPath section { } settings)))
        secretFields;

      # services.eva-kernel: settings — несекретная часть конфига (уедет в
      # мировой стор); секрет — `secrets."<ключ>" = <файл>`: systemd отдаёт
      # файл по LoadCredential, в settings встаёт `{ file = "${credentialsDir}/<ключ>"; }`.
      nixosModule = { config, lib, ... }:
        let
          cfg = config.services.${pname};
          inStore = path: lib.hasPrefix "${builtins.storeDir}/" path;
          # Переопределение ключа конфига окружением (`00 §5`): `EVA_…__PLAINTEXT` — секрет.
          plaintextEnv = name: builtins.match "EVA_.*__PLAINTEXT" (lib.toUpper name) != null;
          # Абсолютные uds-слушатели клиентов держит socket-юнит: права сокета — его
          # дело, ядро берёт готовый сокет по адресу. Относительный путь лежит в
          # закрытом StateDirectory, и группе он всё равно недоступен.
          clientSockets = map (listen: listen.uds)
            (builtins.filter (listen: listen ? uds && lib.hasPrefix "/" listen.uds)
              (cfg.settings.modules.listen or [ ]));
        in
        {
          imports = [
            (eva.lib.mkModule {
              inherit self pname;
              description = "Eva kernel: memory, tools, chats, cron and sleep";
              settingsDefaults = {
                modules.listen = [{ uds = "/run/eva-kernel/modules.sock"; }];
              };
              extraServiceConfig = {
                # На остановке ядро сворачивает идущие туры штатно (наговорённое
                # сохраняется). Дефолтные 90 с systemd добили бы его раньше срока.
                TimeoutStopSec = 180;
                # База хранит ключи сервисов как есть: она, blob'ы и песочницы модулей —
                # только пользователю ядра.
                StateDirectoryMode = "0700";
                UMask = "0077";
                # Сокеты сервисов машины — `/run/eva-<id>/module.sock`, 0660, группа `eva`.
                SupplementaryGroups = [ "eva" ];
              };
            })
          ];

          options.services.${pname}.secrets = lib.mkOption {
            type = lib.types.attrsOf lib.types.str;
            default = { };
            example = {
              "llm.providers.router.api_key" = "/run/secrets/openrouter";
              "modules.tokens.phone.token" = "/run/secrets/eva-phone";
              "modules.services.telegram.key" = "/run/secrets/eva-telegram";
              "hands.endpoints.laptop.key" = "/run/secrets/hands-laptop";
            };
            description = ''
              Secret files by dotted config key. systemd hands each file to
              the service by LoadCredential (readable by root alone is
              enough), and settings get
              `<key> = { file = "${credentialsDir}/<key>"; }`.
            '';
          };

          config = lib.mkIf cfg.enable {
            services.${pname}.settings = lib.mkMerge (lib.mapAttrsToList
              (key: _: lib.setAttrByPath (lib.splitString "." key) {
                file = "${credentialsDir}/${key}";
              })
              cfg.secrets);

            assertions =
              map
                ({ key, ... }: {
                  assertion = false;
                  message = "services.${pname}.settings.${key}: plaintext secrets land in the world-readable nix store; set services.${pname}.secrets.\"${key}\" to the secret file instead";
                })
                (secretsOf lib "plaintext" cfg.settings)
              ++ map
                ({ key, value }: {
                  assertion = !(inStore (toString value));
                  message = "services.${pname}.settings.${key}: ${toString value} is in the world-readable nix store";
                })
                (secretsOf lib "file" cfg.settings)
              ++ lib.mapAttrsToList
                (key: path: {
                  assertion = !(inStore path);
                  message = "services.${pname}.secrets.\"${key}\": ${path} is in the world-readable nix store";
                })
                cfg.secrets
              ++ map
                (name: {
                  assertion = false;
                  message = "services.${pname}.environment.${name}: plaintext secrets land in the world-readable nix store; set services.${pname}.secrets to the secret file instead";
                })
                (builtins.filter plaintextEnv (builtins.attrNames cfg.environment))
              ++ lib.concatMap
                (option: map
                  (path: {
                    assertion = false;
                    message = "services.${pname}.${option}: ${path} is in the world-readable nix store";
                  })
                  (builtins.filter inStore cfg.${option}))
                [ "extraConfigFiles" "environmentFiles" ];

            users.groups.eva = { };

            systemd.sockets.${pname} = lib.mkIf (clientSockets != [ ]) {
              description = "Eva kernel client sockets";
              wantedBy = [ "sockets.target" ];
              socketConfig = {
                ListenStream = clientSockets;
                SocketMode = "0660";
                SocketGroup = "eva";
              };
            };

            systemd.services.${pname} = {
              requires = lib.optional (clientSockets != [ ]) "${pname}.socket";
              after = lib.optional (clientSockets != [ ]) "${pname}.socket";
              serviceConfig.LoadCredential =
                lib.mapAttrsToList (key: path: "${key}:${path}") cfg.secrets;
            };
          };
        };
    in
    flake-utils.lib.eachDefaultSystem
      (system:
        let
          pkgs = eva.lib.mkPkgs system;

          # Конфиг сервиса и проваленные assertions при данных опциях.
          evalService = service:
            let
              nixos = pkgs.nixos {
                imports = [ nixosModule ];
                services.${pname} = { enable = true; } // service;
                boot.loader.grub.enable = false;
                fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
                system.stateVersion = "25.11";
              };
            in
            {
              inherit (nixos.config.services.${pname}) settings;
              inherit (nixos.config.systemd.services.${pname}.serviceConfig)
                LoadCredential SupplementaryGroups StateDirectoryMode UMask;
              inherit (nixos.config.systemd.services.${pname}) requires;
              sockets = nixos.config.systemd.sockets;
              inherit (nixos.config.users) groups;
              failed = map (a: a.message)
                (builtins.filter (a: !a.assertion) nixos.config.assertions);
            };

          secretKeys = [
            "llm.providers.router.api_key"
            "modules.tokens.phone.token"
            "modules.services.telegram.key"
            "hands.endpoints.laptop.key"
          ];
          nonSecret = {
            llm.providers.router.kind = "openai";
            modules.services.telegram.address = "uds:///run/eva-telegram/module.sock";
            hands.endpoints = {
              laptop.address = "10.0.0.2:7777";
              here.local = true;
            };
          };
          # Секрет в settings напрямую: форма `form` со значением `value` по всем ключам.
          inlined = form: value: pkgs.lib.foldl' pkgs.lib.recursiveUpdate nonSecret
            (map (key: pkgs.lib.setAttrByPath (pkgs.lib.splitString "." key) { ${form} = value; })
              secretKeys);
        in
        {
          packages = rec {
            eva-kernel = service pkgs "eva-kernel";
            default = eva-kernel;

            docs = pkgs.stdenvNoCC.mkDerivation {
              pname = "eva-kernel-docs";
              version = commit;
              src = self;
              nativeBuildInputs = [ pkgs.mdbook pkgs.mdbook-mermaid ];
              buildPhase = ''
                runHook preBuild
                mdbook build docs --dest-dir "$out"
                runHook postBuild
              '';
              dontInstall = true;
              dontFixup = true;
            };
          };

          checks.nixos-secrets =
            let
              lib = pkgs.lib;
              byFile = evalService {
                settings = nonSecret;
                secrets = lib.genAttrs secretKeys (key: "/run/secrets/${key}");
              };
              plaintext = evalService { settings = inlined "plaintext" "sk-or-secret"; };
              inStore = evalService {
                settings = inlined "file" "${builtins.storeDir}/0000-secret";
                secrets."modules.tokens.other.token" = "${builtins.storeDir}/0000-token";
              };
              # Тот же секрет мимо settings: переменной окружения и файлами из стора.
              sideways = evalService {
                settings = nonSecret;
                environment = {
                  EVA_MODULES__TOKENS__BRIDGE__TOKEN__PLAINTEXT = "sk-secret";
                  eva_llm__providers__router__api_key__plaintext = "sk-secret";
                  EVA_LLM__PROVIDERS__ROUTER__API_KEY__FILE = "/run/secrets/router";
                };
                extraConfigFiles = [ "${builtins.storeDir}/0000-secrets.yaml" "/run/secrets/eva.yaml" ];
                environmentFiles = [ "${builtins.storeDir}/0000-env" ];
              };
            in
            assert byFile.failed == [ ];
            assert byFile.settings.modules.listen == [{ uds = "/run/eva-kernel/modules.sock"; }];
            assert byFile.groups ? eva;
            assert builtins.elem "eva" byFile.SupplementaryGroups;
            # База с ключами сервисов — только пользователю ядра; сокет клиентов
            # группе `eva` отдаёт socket-юнит, а не процесс.
            assert byFile.StateDirectoryMode == "0700";
            assert byFile.UMask == "0077";
            assert byFile.sockets.${pname}.socketConfig.ListenStream == [ "/run/eva-kernel/modules.sock" ];
            assert byFile.sockets.${pname}.socketConfig.SocketMode == "0660";
            assert byFile.sockets.${pname}.socketConfig.SocketGroup == "eva";
            assert builtins.elem "${pname}.socket" byFile.requires;
            # Относительный сокет живёт в закрытом StateDirectory: socket-юнит не нужен.
            assert !((evalService { settings = nonSecret // { modules.listen = [{ uds = "modules.sock"; }]; }; }).sockets ? ${pname});
            assert byFile.settings.llm.providers.router.api_key
              == { file = "${credentialsDir}/llm.providers.router.api_key"; };
            assert byFile.settings.modules.tokens.phone
              == { token.file = "${credentialsDir}/modules.tokens.phone.token"; };
            assert byFile.settings.modules.services.telegram == {
              address = "uds:///run/eva-telegram/module.sock";
              key.file = "${credentialsDir}/modules.services.telegram.key";
            };
            assert byFile.settings.hands.endpoints.laptop.key.file
              == "${credentialsDir}/hands.endpoints.laptop.key";
            assert lib.sort lib.lessThan byFile.LoadCredential
              == lib.sort lib.lessThan (map (key: "${key}:/run/secrets/${key}") secretKeys);
            assert builtins.length plaintext.failed == 4;
            assert builtins.all (m: lib.hasInfix "plaintext secrets" m) plaintext.failed;
            assert builtins.length inStore.failed == 5;
            assert builtins.length sideways.failed == 4;
            assert builtins.length (builtins.filter (lib.hasInfix "__PLAINTEXT") sideways.failed) == 1;
            assert builtins.length (builtins.filter (lib.hasInfix "__plaintext") sideways.failed) == 1;
            assert builtins.any (lib.hasInfix "extraConfigFiles: ${builtins.storeDir}/0000-secrets.yaml") sideways.failed;
            assert builtins.any (lib.hasInfix "environmentFiles: ${builtins.storeDir}/0000-env") sideways.failed;
            pkgs.runCommand "nixos-secrets" { } "touch $out";

          devShells.default = eva.lib.mkDevShell {
            inherit pkgs;
            extraPackages = [ pkgs.sqlite pkgs.cargo-insta pkgs.mdbook pkgs.mdbook-mermaid ];
          };
        })
    // {
      nixosModules.default = nixosModule;
    };
}
