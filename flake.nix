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
    in
    flake-utils.lib.eachDefaultSystem
      (system:
        let
          pkgs = eva.lib.mkPkgs system;
        in
        {
          packages = rec {
            eva-kernel = service pkgs "eva-kernel";
            default = eva-kernel;
          };

          devShells.default = eva.lib.mkDevShell {
            inherit pkgs;
            extraPackages = [ pkgs.sqlite pkgs.cargo-insta ];
          };
        })
    // {
      # services.eva-kernel: settings — несекретная часть конфига (уедет в
      # мировой стор), ключи провайдеров и токены модулей — отдельными
      # файлами в extraConfigFiles либо через environmentFiles.
      nixosModules.default = eva.lib.mkModule {
        inherit self;
        pname = "eva-kernel";
        description = "Eva kernel: memory, tools, chats, cron and sleep";
        settingsDefaults = {
          modules.listen = [{ uds = "/run/eva-kernel/modules.sock"; }];
        };
        extraServiceConfig = {
          # На остановке ядро сворачивает идущие туры штатно (наговорённое
          # сохраняется). Дефолтные 90 с systemd добили бы его раньше срока.
          TimeoutStopSec = 180;
          RuntimeDirectory = "eva-kernel";
          RuntimeDirectoryPreserve = true;
        };
      };
    };
}
