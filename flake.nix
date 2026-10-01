{
  description = "Eva kernel: the runtime she lives in, plus its protocol, SDK and services";

  inputs.eva.url = "git+https://git.desu.church/VienDesu/eva.git";

  outputs = { self, eva, ... }: eva.lib.mkServiceFlake {
    inherit self;
    pname = "eva-kernel";
    description = "Eva kernel: memory, tools, chats, cron and sleep";
    env = {
      # Один воркспейс, несколько бинарей: `--package` выбирает, что установить.
      cargoExtraArgs = "--locked --package eva-kernel";
      # Ревизия в бинарь: .git в песочницу сборки не едет, а знать, что
      # именно крутится, нужно и Еве (промпт), и метрикам.
      EVA_COMMIT = self.shortRev or self.dirtyShortRev or "dev";
    };
    # Контракт клиентов по wss поднимает TLS на сертификатах из tests/fixtures.
    srcFilter = path: _: builtins.match ".*/tests/fixtures/.*" path != null;
    settingsDefaults = {
      modules.listen = [{ uds = "/run/eva-kernel/modules.sock"; }];
    };
    extraServiceConfig = {
      # На остановке ядро сворачивает идущие туры штатно (наговорённое
      # сохраняется). Дефолтные 90 с systemd добили бы его раньше срока.
      TimeoutStopSec = 180;
      # Каталог клиентского сокета из settingsDefaults.
      RuntimeDirectory = "eva-kernel";
      # База хранит ключи сервисов как есть: она, blob'ы и песочницы модулей —
      # только пользователю ядра.
      StateDirectoryMode = "0700";
      UMask = "0077";
      # Сокеты сервисов машины — `/run/eva-<id>/module.sock`, группа `eva`.
      SupplementaryGroups = [ "eva" ];
    };
    secretFields = [
      "llm.providers.*.api_key"
      "modules.tokens.*.token"
      "modules.services.*.key"
      "hands.endpoints.*.key"
    ];
    devShell = pkgs: {
      extraPackages = [ pkgs.sqlite pkgs.cargo-insta pkgs.mdbook pkgs.mdbook-mermaid ];
    };
  };
}
