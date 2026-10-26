{
  description = "Eva soul: the runtime she lives in, plus its protocol, SDK and services";

  inputs.eva.url = "git+ssh://git@github.com/nerodono/eva.git";

  outputs = { self, eva, ... }: eva.lib.mkServiceFlake {
    inherit self;
    pname = "eva-soul";
    description = "Eva soul: memory, tools, chats, cron and sleep";
    env = {
      # Один воркспейс, несколько бинарей: `--package` выбирает, что установить.
      cargoExtraArgs = "--locked --package eva-soul";
      # Ревизия в бинарь: .git в песочницу сборки не едет, а знать, что
      # именно крутится, нужно и Еве (промпт), и метрикам.
      EVA_COMMIT = self.shortRev or self.dirtyShortRev or "dev";
    };
    # Контракт клиентов по wss поднимает TLS на сертификатах из tests/fixtures.
    srcFilter = path: _: builtins.match ".*/tests/fixtures/.*" path != null;
    settingsDefaults = {
      modules.listen = [{ uds = "/run/eva-soul/modules.sock"; }];
    };
    extraServiceConfig = {
      # На остановке soul сворачивает идущие туры штатно (наговорённое
      # сохраняется). Дефолтные 90 с systemd добили бы его раньше срока.
      TimeoutStopSec = 180;
      # Каталог клиентского сокета из settingsDefaults.
      RuntimeDirectory = "eva-soul";
      # База хранит ключи сервисов как есть: она, blob'ы и песочницы модулей —
      # только пользователю soul.
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
      extraPackages = [ pkgs.sqlite pkgs.cargo-insta pkgs.mdbook pkgs.mdbook-mermaid pkgs.gettext ];
    };
  };
}
