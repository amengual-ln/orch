# orch

Orquestador de agentes de código locales, orientado a una TUI manejada desde el teclado. El alcance y las fases están en [PLAN.md](PLAN.md).

**Estado: 1B implementada.** Hay un runner headless Codex y una CLI de prueba de dos turnos. Los protocolos de Codex y agy/Gemini tienen capturas reales; el runner agy corresponde a 1C. Todavía no hay TUI ni controlador de sesiones concurrentes.

## Requisitos

- Linux.
- Rust y Cargo **1.100 o posterior**, edición 2024. La versión mínima verificada es 1.100.0; `Cargo.lock` fija las dependencias del binario.
- Python 3 para las pruebas offline de procesos, usando solo su biblioteca estándar.
- Para el smoke test real: Codex instalado, autenticación válida, acceso al servicio y directorio de estado local escribible. Se verificó con `codex-cli 0.172.0`.

Durante 1B se instaló el toolchain oficial mínimo más rustfmt/Clippy en `/tmp/orch-rust`, sin modificar perfiles de shell. Para usar esa instalación temporal en esta máquina:

```sh
export CARGO_HOME=/tmp/orch-rust/cargo
export RUSTUP_HOME=/tmp/orch-rust/rustup
export PATH="/tmp/orch-rust/cargo/bin:$PATH"
```

Si ya tenés Rust instalado normalmente, no necesitás esas variables. La instalación en `/tmp` puede desaparecer; los requisitos permanentes son los de arriba.

## Compilar y verificar

```sh
cargo check --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

Los tests usan fixtures y procesos falsos, sin invocar modelos ni consumir cuota. La primera descarga de dependencias necesita acceso a crates.io; después pueden ejecutarse con `--offline`.

Las reglas permanentes están en [AGENTS.md](AGENTS.md). CI exige **90% de cobertura de líneas de producción** y pruebas de los flujos críticos; no se excluyen módulos de `src/` para subir el porcentaje. Se excluye del cálculo únicamente `tests/`.

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked --version 0.9.2
cargo llvm-cov --locked --all-targets --ignore-filename-regex '(^|/)tests/' --fail-under-lines 90
```

Medición del 26 de noviembre de 2026: **94,75%** (415 de 438 líneas), con **14 tests offline**. Incluyen mensajes reemplazados sin duplicación, secuencias inválidas, límites acumulados de memoria, limpieza al descartar el futuro, consumidor de eventos fallido y timeout de la CLI, además de la cobertura inicial de procesos/señales. La cobertura de líneas no mide por sí sola todas las combinaciones de ramas o carreras.

[El workflow](.github/workflows/ci.yml) prepara Rust 1.100.0 y ejecuta formato, check, Clippy, tests y el umbral de cobertura en pushes y pull requests. Los comandos se verificaron localmente en el host; los resultados de cada ejecución remota se consultan en GitHub Actions y en el PR correspondiente.


La prueba de SIGINT/SIGTERM de la CLI pasó en el host Linux y falló en el entorno restringido de las herramientas de esta sesión: Tokio no notificó esas señales al monitor allí. No se omitió la prueba ni se relajaron sus comprobaciones; la suite completa se verificó en el host. Un entorno de ejecución debe permitir señales entre los procesos de prueba.

## Smoke test real de Codex

Desde el directorio donde querés guardar los logs de orch:

```sh
cargo run --locked -- --smoke-test codex --cwd /ruta/al/proyecto
```

Opciones adicionales:

```sh
cargo run --locked -- --smoke-test codex \
  --cwd /ruta/al/proyecto --timeout-secs 120 --codex-bin /ruta/al/codex
```

La prueba hace dos llamadas reales y puede consumir cuota:

1. Abre una conversación, pide recordar un marcador único y responder `OK`.
2. Reanuda por el ID explícito y pide recuperar el marcador, sin incluirlo en el segundo prompt.

Cada proceso recibe el cwd de la sesión mediante `Command::current_dir`, argumentos separados, stdin cerrado, sandbox `read-only`, aprobaciones `never` y `--ignore-user-config`. No se usan `--last`, `--ephemeral` ni bypass. Esta opción omite la configuración de usuario de Codex, pero no elimina todas las instrucciones que pueda cargar por otras vías; la autenticación sigue siendo la del CLI.

El timeout es por turno, 120 segundos por defecto, y admite valores entre 1 y 86400. `--codex-bin` permite elegir otra instalación del ejecutable; no interpreta comandos de shell.

## Eventos y resultados

stdout contiene JSONL normalizado: cada evento tiene `session_id`, `turn_id` y `event`. Los eventos implementados son `turn_started`, `session_initialized`, `message_completed`, `turn_completed`, `turn_failed` y `turn_cancelled`.

Los mensajes completos son contenido de conversación; `turn_completed.final_text` consolida el resultado y no es otro mensaje para agregar al historial. No se inventan deltas cuando el proveedor entrega mensajes completos.

El runner confirma fin de protocolo, identidad de la conversación y exit code 0 antes de emitir `turn_completed`. Emite un único resultado terminal por turno aceptado. La CLI además verifica las respuestas del smoke test: un turno del proveedor puede completarse correctamente pero fallar la comprobación de `OK` o del marcador, en cuyo caso la CLI sale con error.

- Exit code 0: ambos turnos y la comprobación de continuidad pasaron.
- Exit code 1: configuración, lanzamiento, proveedor, protocolo, timeout o comprobación del smoke test fallidos.
- Exit code 130: cancelación por Ctrl+C o SIGTERM, tras limpiar la ejecución controlada.
- Los errores de argumentos los informa clap con exit code 2.

Los diagnósticos de la CLI van a stderr; los logs operativos se escriben en `.orch/orch.log` **relativo al cwd de arranque de orch**, no al proyecto de la sesión. Los logs no guardan prompts ni líneas crudas del proveedor. Los archivos nuevos de log tienen permisos 0600 y el directorio nuevo 0700; ambos están ignorados por Git junto con `target/`.

## Procesos y límites actuales

stdout y stderr se drenan concurrentemente. Se limitan las líneas stdout a 1 MiB, el texto/IDs de mensajes acumulados a 4 MiB y el diagnóstico stderr retenido a los últimos 16 KiB de bytes. JSON inválido, UTF-8 inválido y eventos desconocidos no causan panic ni prueban éxito. Un stream sin fin válido falla o vence por timeout.

Cada ejecución tiene grupo de procesos propio. Cancelación y timeout envían SIGINT, conceden 300 ms de gracia, escalan a SIGKILL y recolectan al hijo directo. También se termina el grupo restante al salir normalmente el líder. El apagado incluye límites para recolección y drenaje de pipes. La guarda de grupo y `kill_on_drop` sirven de respaldo si se descarta el futuro.

La garantía cubre procesos del grupo controlado. Descendientes que se desacoplen quedan fuera; tampoco se promete recolectar nietos como hijos propios. Los tests comprueban que el hijo directo ya fue recolectado y que los descendientes controlados no siguen ejecutándose, admitiendo un zombi a cargo de su nuevo padre.

El parser Codex de 1B se centra en conversación, mensajes completos y cierre de turno; requiere al menos un mensaje del agente para un turno satisfactorio. Todavía no normaliza actividad de herramientas o deltas de Codex, ni habilita ediciones. Los errores de proveedor usados en tests son sintéticos donde no había captura real de ese esquema.

## Evidencia de las fases iniciales

| Verificación | Resultado |
| --- | --- |
| 1A, Codex 0.172.0 (2026-11-25) | Dos turnos reales con mismo `thread_id`, marcador recuperado y exit code 0. |
| 1A-agy, agy 1.5.17 (2026-11-26) | Gemini `gemini-3.8-flash-low`: reanudación por ID y dos turnos en un proceso vivo, deltas reales y errores controlados. |
| 1B, runner Rust (2026-11-26) | Smoke real de dos turnos con exit code 0 e ID conservado; cwd de ambos procesos comprobado mediante `/proc`. |
| Verificación 1B | `cargo check`, ocho tests offline iniciales en el host, rustfmt y Clippy sin warnings; la suite ampliada tiene 14 tests y 94,75% de cobertura. |

Detalles y comandos directos de los proveedores: [fixtures Codex](tests/fixtures/codex-0.172.0/README.md) y [fixtures agy/Gemini](tests/fixtures/agy-1.5.17/README.md). Las capturas de 1A son históricas; las notas de ausencia de proyecto Rust describen ese momento.

## Segundo proveedor: agy/Gemini, siguiente paso

`agy` identifica el ejecutable/protocolo; Gemini es el modelo seleccionado para esa sesión. `agy models` devolvió las variantes Flash `high`, `medium` y `low`, entre otras; solo se probó `gemini-3.8-flash-low`. No se debe asumir disponibilidad futura ni hacer fallback silencioso.

| Aspecto | Codex 0.172.0 | agy 1.5.17 / Gemini probado |
| --- | --- | --- |
| Primer proceso | `exec --json` | `--print ... --output-format stream-json` |
| Reanudación explícita | `exec resume <thread_id>` | `--conversation <conversation_id>` |
| Discriminante JSONL | `type` | `event` |
| Texto observado | `item.completed` con mensaje completo | `step_update.text_delta`, incluso el último `DONE` |
| Resultado | `turn.completed`, texto en mensajes previos | `result.status` y `result.response` consolidado |
| Proceso vivo con varios turnos | No probado | Verificado con `--input-format stream-json` |
| Permisos probados | Sandbox solicitado `read-only`, aprobaciones `never` | `strict`; rechazo headless de un comando inocuo confirmado |

En agy se observó `SUCCESS` y exit code 0 con una herramienta denegada y `denied_actions`: el runner de 1C deberá clasificarlo como fallo de permisos. Esta conducta también se describe en el [modo headless oficial](https://antigravity.google/docs/cli/headless/). Su flag `--sandbox` no es una equivalencia probada del `read-only` de Codex. Además, `--disable-slash-commands` anula `--mode plan` con un warning; las capturas principales evitan combinarlos.

**Próximo paso: 1C**, incorporar el parser y runner agy/Gemini, reutilizando el control de procesos. El MVP usará un proceso por turno con reanudación explícita para ambos proveedores, antes del controlador concurrente y la TUI.


## Repositorio

`origin` está configurado como `https://github.com/amengual-ln/orch.git`, con `main` como rama predeterminada. Se creó un commit base vacío para inicializarla antes de activar las protecciones; el proyecto se publica mediante una rama y PR.

`main` exige PR y el check `quality` de GitHub Actions en verde, con la rama actualizada respecto de `main`. No requiere aprobaciones de otras personas; el autor puede mergear su propio PR cuando pasa CI. No hay excepciones de bypass, tampoco para el administrador; force-push y borrado están bloqueados.

La configuración versionada está en [main-ruleset.json](.github/main-ruleset.json). Editar el archivo no cambia GitHub automáticamente: hay que aplicar y verificar la regla remota. Las instrucciones permanentes están en [AGENTS.md](AGENTS.md).
