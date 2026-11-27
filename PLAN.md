# PLAN.md — Orquestador TUI keyboard-driven (`orch`)

## 1. Objetivo y alcance inicial

Construir `orch`, una CLI/TUI escrita en Rust para lanzar y gestionar sesiones concurrentes de agentes de código locales desde el teclado, con navegación modal estilo Vim/tmux.

La primera versión usable debe permitir crear sesiones Codex y agy/Gemini con distintos directorios de trabajo, enviar y reanudar turnos, ver respuestas, alternar entre sesiones y cancelar ejecuciones sin bloquear la interfaz. El flujo de aceptación incluye una sesión de cada proveedor en paralelo.

El orden de trabajo es: validar los protocolos reales, implementar primero Codex y luego agy/Gemini, controlar correctamente sus procesos y turnos, y probar la ergonomía de una TUI mínima. Después se incorporarán Claude, persistencia y comunicación entre sesiones. MCP, memoria global, skills e imágenes siguen en el horizonte, pero no bloquean el primer flujo usable.

Plataforma inicial: Linux. La compatibilidad con otros sistemas se abordará cuando exista un flujo probado; en particular, no asumir que la gestión de procesos es portable.

## 2. Decisiones para las bases

### Dependencias y estructura

- Rust estable: edición 2024, versión mínima verificada 1.100. `Cargo.lock` fija las dependencias del binario.
- `tokio` para procesos, IO, canales, señales y timeout; habilitar solo las features utilizadas. `nix` para señalizar grupos de procesos Linux sin introducir bloques unsafe propios.
- `serde` + `serde_json` para los eventos JSONL del proveedor y los eventos internos.
- `clap` para argumentos y `anyhow` para errores del binario. Agregar errores tipados donde el controlador necesite distinguirlos; no sumar bibliotecas por anticipado.
- `tracing` + `tracing-subscriber` para diagnóstico en archivo, separado de la salida visible.
- `ratatui` + `crossterm` al empezar la TUI.
- Agregar configuración TOML, SQLite, SDK MCP y soporte de imágenes en sus respectivas fases.

Estructura inicial orientativa; crear archivos a medida que hagan falta:

```text
Cargo.toml
PLAN.md
README.md
src/
├── main.rs          # CLI, smoke test y apagado
├── events.rs        # Contrato de eventos
├── codex.rs         # Comandos y traducción del protocolo Codex
├── process.rs       # IO, timeout, cancelación y recolección del proceso
├── session.rs       # Estado y controlador por sesión; fase 2
└── tui/             # Estado, teclas y renderizado; fase 3
tests/
└── fixtures/        # Salidas JSONL anonimizadas y versión del proveedor
```

No introducir un trait `AgentRunner` ni una jerarquía de adaptadores hasta integrar agy en 1C. Agregar entonces `agy.rs` y extraer solo el contrato que ambos necesitan. El contrato de eventos y el control de procesos deben poder reutilizarse sin anticipar todas las diferencias entre CLIs.

### Sesiones y turnos

- Separar el identificador interno de sesión del identificador de conversación del proveedor.
- Cada sesión tiene proveedor, modelo seleccionado (opcional si se usa el default del CLI), alias único dentro de la instancia de orch, cwd absoluto validado, estado, historial e identificador del proveedor cuando esté disponible. `agy` identifica el ejecutable/protocolo; Gemini identifica la familia del modelo elegido, no otro tipo de runner.
- Cada turno tiene un identificador propio y conserva su prompt, resultado y motivo de terminación.
- Estados de sesión iniciales: `Idle`, `Running`, `Cancelling`, `Error`, `Closed`. Una cancelación confirmada vuelve a `Idle`; un fallo queda visible y permite intentar otro turno explícitamente.
- Un controlador por sesión es el único responsable de iniciar turnos y modificar su estado. Recibe comandos mediante un canal acotado y permite como máximo un turno activo.
- Un nuevo envío mientras la sesión está ocupada devuelve `busy`; inicialmente no hay cola de prompts ni reintentos automáticos.
- El store es una vista del estado, no otro ejecutor. Nunca mantener un lock mientras se espera al proveedor, un proceso o una consulta entre sesiones.
- Usar canales acotados. Los eventos de control y terminación no se descartan; la UI puede agrupar renderizados y recibir snapshots. Evitar que un suscriptor lento pierda el resultado por depender de un `broadcast` sin recuperación.

### Contrato de eventos

Cada evento lleva `session_id` y `turn_id`. El controlador genera `TurnStarted` y exactamente un resultado terminal por turno aceptado:

- `SessionInitialized { provider_session_id }`: conversación identificada por el proveedor.
- `TextDelta { text }`: contenido incremental cuando el proveedor realmente lo emite.
- `MessageCompleted { message_id, text }`: mensaje completo; no implica que terminó el turno.
- `ToolExecution { tool_name, summary, status }`: actividad visible de herramientas, incluyendo fallo o permiso denegado; no confundir un intento con una ejecución exitosa.
- `TurnCompleted { final_text }`: resultado satisfactorio confirmado.
- `TurnFailed { kind, message }`: fallo de proveedor, proceso, protocolo, timeout o permisos.
- `TurnCancelled`: cancelación confirmada tras finalizar la ejecución controlada.

Los nombres y campos concretos se ajustarán a las muestras de la fase 1. No inventar streaming si solo hay mensajes completos. Definir cómo un mensaje completo reemplaza sus deltas para no mostrar texto duplicado; `final_text` representa el resultado consolidado, no otro mensaje agregado al historial.

Un mensaje del agente, EOF o exit code 0 por sí solos no prueban éxito. Para el runner Codex por invocación, confirmar el fin satisfactorio del protocolo y la salida correcta del proceso antes de publicar `TurnCompleted`. EOF sin cierre válido, errores del proveedor y códigos de salida no exitosos producen fallo. Resolver carreras entre fin y cancelación en el controlador para emitir un único resultado terminal.

Para agy, usar `result.status` y revisar también `result.denied_actions`: se observó `SUCCESS` con exit code 0 y una herramienta denegada. En el MVP, un resultado con acciones denegadas se normaliza como `TurnFailed` de permisos, conservando texto parcial y diagnóstico; no reportar la tarea como realizada. Los deltas de `step_update` y el texto completo de `result.response` no se concatenan entre sí.

Decisión inicial: un proceso por turno en ambos runners; agy reanuda con `--conversation <id>`. Se verificaron dos turnos en un proceso agy mediante stdin JSONL, pero esa optimización queda fuera del MVP. Si se incorpora, `result` termina el turno sin terminar el proceso: no esperar exit code por cada turno vivo y tratar el cierre inesperado como fallo de la sesión.

### Procesos, cwd y permisos

- Lanzar ejecutables con argumentos separados, sin construir comandos de shell a partir de prompts o rutas.
- Cada lanzamiento y reanudación usa `Command::current_dir` con el cwd de su sesión. Nunca cambiar el directorio global de orch.
- Dos sesiones con el mismo cwd comparten archivos y pueden interferirse. Mostrar esta condición; worktrees y aislamiento automático quedan fuera del MVP.
- Leer stdout y stderr concurrentemente hasta su cierre. stdout contiene el protocolo; stderr aporta diagnóstico y no se interpreta como JSONL.
- Limitar tamaños de líneas y buffers de diagnóstico. Ignorar eventos desconocidos y registrar líneas inválidas sin panic; un stream que nunca completa válidamente termina por fallo o timeout.
- En Linux, controlar cada ejecución con un grupo de procesos propio: señal de interrupción, plazo de gracia, terminación forzada si es necesaria y `wait` para recolectar el hijo directo. Verificar también descendientes del grupo; `kill_on_drop` es un respaldo, no toda la estrategia. Procesos que se desacoplen del grupo quedan fuera de esa garantía y deben documentarse.
- Timeout explícito por turno. Al vencer, detener y recolectar la ejecución antes de reportar el fallo. Ctrl+C y cierre de la TUI usan la misma ruta de apagado.
- Los smoke tests iniciales son de solo lectura. Antes de habilitar ediciones, definir y verificar una política explícita de sandbox/permisos para lanzamiento y reanudación; no asumir que ambos aceptan las mismas opciones.
- En el MVP headless no habrá un diálogo de aprobaciones propio: usar una política no interactiva soportada por el proveedor que rechace lo no autorizado y mostrar ese rechazo como fallo accionable. No activar bypass de permisos para hacer pasar una prueba. Si una versión no permite esta política, documentar la limitación antes de habilitar ediciones.

## 3. Fases iniciales

### Fase 1 — Pruebas verticales headless: Codex y agy/Gemini

Objetivo: comprobar continuidad de conversación, eventos reales y ciclo de vida de procesos antes de construir la TUI.

#### 1A. Verificar el entorno y el contrato real

- [x] Comprobar `rustc`, `cargo` y `codex`; documentar versiones y requisitos en README.
- [x] Verificar los `--help` de `codex exec` y `codex exec resume`, incluyendo permisos, cwd y JSONL. Las invocaciones verificadas para 0.172.0 están en README.
- [x] Capturar una conversación de dos turnos de solo lectura: el primero recibe un marcador y el segundo debe recordarlo sin reenviarlo en el prompt.
- [x] Guardar fixtures anonimizadas y documentar qué eventos identifican conversación, mensajes, fin satisfactorio y errores. No guardar credenciales ni contenido privado.

Resultado 1A (2026-11-25): Codex 0.172.0 completó ambos turnos con el mismo `thread_id`, recuperó el marcador sin repetirlo en el segundo prompt y terminó con exit code 0 en ambos procesos. Ver [README](README.md) y [fixtures](tests/fixtures/codex-0.172.0/README.md). El entorno restringido impedía escribir el estado local del CLI; las capturas satisfactorias se obtuvieron con acceso al host autorizado y sandbox del agente solicitado como `read-only`, aprobaciones `never` y sin herramientas. Se capturaron errores previos al turno; el esquema de errores durante un turno y la aplicación de permisos con herramientas siguen pendientes. `cargo` y `rustc` no aparecen en el PATH examinado. No se creó el proyecto Rust ni se instaló el toolchain.

Observación del análisis inicial: `claude` y `codex` estaban en el PATH; `agy`, `cargo` y `rustc` no. Revalidar antes de trabajar: es un dato del entorno, no una restricción del proyecto.

#### 1A-agy. Verificar el segundo protocolo antes de implementar

- [x] Verificar agy, su ayuda, selección de modelo y requisitos de ejecución.
- [x] Consultar `agy models` y fijar un Gemini disponible para las pruebas: `gemini-3.8-flash-low` en agy 1.5.17.
- [x] Capturar un turno y su reanudación por `conversation_id`, recuperando un marcador sin reenviarlo en el segundo prompt.
- [x] Verificar dos turnos en un proceso vivo con stdin JSONL y flush; enviar el segundo después del primer `result` y cerrar stdin al terminar.
- [x] Capturar errores por modelo inexistente y bloque de entrada no admitido, y un rechazo real de permisos en headless.
- [x] Guardar fixtures anonimizadas, diferencias con Codex y límites de permisos/sandbox.

Resultado (2026-11-26): agy 1.5.17 ejecutó Gemini con `permission_mode: strict`. La reanudación mantuvo el ID y el marcador; el proceso persistente emitió dos resultados y siguió vivo hasta cerrar stdin. Se observaron deltas reales, `result.status: ERROR` y `SUCCESS` con `denied_actions` ante una herramienta denegada. `--disable-slash-commands` anula `--mode plan` con un warning; las capturas principales evitan esa combinación. Ver [fixtures agy](tests/fixtures/agy-1.5.17/README.md). El aislamiento OS efectivo de `--sandbox`, cancelación y descendientes siguen pendientes; no equiparar el modo plan con sandbox read-only. No se creó proyecto Rust.

#### 1B. Implementar el mínimo ejecutable con Codex

- [x] Inicializar Cargo con las dependencias necesarias para esta fase.
- [x] Implementar el parser Codex como traducción testeable, separado del lanzamiento de procesos.
- [x] Implementar `--smoke-test codex --cwd <ruta>`: ejecutar los dos turnos de continuidad, imprimir eventos normalizados y devolver exit code no cero ante fallo.
- [x] Validar el cwd antes del lanzamiento y reutilizar el identificador del proveedor al reanudar. No elegir automáticamente la conversación más reciente.
- [x] Implementar lectura concurrente de stdout/stderr, timeout, cancelación, escalado de señales y recolección del proceso desde esta fase.
- [x] Configurar logs fuera de stdout; limitar y evitar registrar contenido sensible innecesario. Resolver la ruta de `.orch/orch.log` desde el cwd de arranque de orch, independientemente del cwd de cada sesión.

#### Validación y aceptación 1B

- [x] Fixtures cubren identificación de conversación, mensaje, fin, error y eventos desconocidos/líneas inválidas; no se confunde mensaje con fin de turno ni se duplica contenido.
- [x] Un proceso de prueba determinista permite comprobar stderr abundante, salida no cero, EOF incompleto, timeout y cancelación con un descendiente en el mismo grupo. Estas pruebas no dependen del modelo ni consumen llamadas.
- [x] El smoke test real conserva contexto entre dos turnos y usa el cwd solicitado en ambos lanzamientos.
- [x] Un ejecutable ausente, una autenticación inválida o un cwd inválido producen un error accionable; nunca un falso éxito.
- [x] Tras cancelar o vencer el timeout, el hijo directo queda recolectado y los descendientes controlados del grupo no siguen ejecutándose.
- [x] Pasan `cargo check` y `cargo test`. La prueba real con proveedor es explícita y separada de los tests offline.

**Entrega 1B:** binario headless Codex verificable, README y contrato sustentado en fixtures. No requiere MCP, persistencia, configuración global ni implementar todavía agy.

Resultado 1B (2026-11-26): proyecto Cargo edición 2024 y Rust mínimo verificado 1.100; toolchain instalado temporalmente en `/tmp/orch-rust`, sin modificar el shell. El smoke real del runner completó dos turnos, mantuvo el ID y recuperó el marcador; se comprobaron los cwd de ambos hijos mediante `/proc`. Las ocho pruebas offline cubren fixtures, errores, límites, concurrencia de pipes, reanudación, SIGINT/SIGTERM, timeout y limpieza de grupos. Las pruebas de señales de la CLI fallan en el entorno restringido de estas herramientas y pasan en el host: la suite completa se verifica allí, sin saltarlas. Pasan `cargo check`, `cargo test`, rustfmt y Clippy con warnings denegados. No se probaron ediciones ni políticas de herramientas reales de Codex; el parser inicial requiere un mensaje final y no normaliza todavía herramientas/deltas. Ver [README](README.md).

#### 1C. Agregar agy/Gemini como segundo runner

- [x] Después de validar 1B, implementar el parser agy sobre las fixtures 1A-agy y extraer el contrato mínimo común con Codex.
- [x] Agregar `--smoke-test agy --model <slug> --cwd <ruta>`; mantener `--smoke-test codex`. Fijar el modelo explícitamente en las pruebas de Gemini y repetirlo al reanudar.
- [x] Ejecutar un proceso por turno con `--print`, `--output-format stream-json` y `--conversation <id>` para continuar; usar el cwd del proceso, no `--project` como sustituto.
- [x] Mantener las opciones de sesión verificadas (`--mode plan --sandbox --settings '{"toolPermission":"strict"}'`) durante las pruebas iniciales; no activar auto/accept-edits ni bypass para evitar rechazos. Separar rechazo de permisos de fallos de protocolo.
- [x] Probar errores sin `init`/ID, `step_update` incremental, texto final sin duplicación, `denied_actions` y exit codes no exitosos. Verificar cwd/modelo/permisos declarados en `init`.
- [x] Aplicar al runner agy los controles de timeout, cancelación y recolección de procesos de 1B, considerando el servidor local que inicia el CLI. No dar por probada la limpieza por observar que salió el proceso principal.

**Aceptación 1C:** ambos smoke tests reales conservan conversación y cwd; agy usa el Gemini solicitado. Las pruebas offline distinguen éxito, error y permisos denegados sin duplicar respuestas. Pasan `cargo check` y `cargo test`. Las pruebas de ediciones se habilitan solo después de verificar una política específica para cada proveedor.

Resultado 1C (2026-11-27): ambos smoke tests reales completaron dos turnos, recuperaron el marcador y mantuvieron el ID. El cwd se comprobó mediante `/proc` en los cuatro lanzamientos, y agy confirmó `gemini-3.8-flash-low` y permisos `strict`. En el primer intento ambos turnos iniciales vencieron. agy informó `trajectory not found` y dejó un proceso en otro grupo fuera de la limpieza de orch. Las pruebas offline de señales verifican el hijo directo y el descendiente controlado de ambos runners. Ver [README](README.md).

### Fase 2 — Control de sesiones concurrentes

Objetivo: administrar dos sesiones independientes usando los runners validados.

- [ ] Implementar sesión, identificadores internos y controlador por sesión con comandos `SendTurn`, `Cancel` y `Close`.
- [ ] Validar alias únicos y cwd accesible al crear la sesión.
- [ ] Mantener historial y estado en memoria; conservar borrador y scroll por sesión cuando se agregue la UI.
- [ ] Rechazar un segundo turno sobre una sesión ocupada; permitir que otra sesión avance en paralelo.
- [ ] Correlacionar cada evento con sesión y turno. Cancelar una sesión no afecta a las demás.
- [ ] Definir la recuperación tras cancelación/fallo: conservar el ID del proveedor si existe; no reintentar ni crear otra conversación silenciosamente si la reanudación falla.
- [ ] Implementar apagado global con plazo acotado para todos los controladores.

**Aceptación:** pruebas offline con dos sesiones y procesos controlados verifican concurrencia, rechazo `busy`, asociación de eventos, cancelación selectiva y apagado. Una prueba real con una sesión Codex y otra agy/Gemini, de dos turnos cada una, verifica continuidad, selección de modelo y cwd en dos directorios distintos.

### Fase 3 — TUI mínima para iterar la ergonomía

Objetivo: completar el flujo diario básico desde el teclado con Codex y agy/Gemini.

- [ ] Agregar `ratatui` y `crossterm`; integrar teclado y eventos de sesiones sin bloquear el renderizado ni la entrada.
- [ ] Layout inicial: lista de sesiones a la izquierda, conversación e input a la derecha, barra de estado con modo, estado y atajos. Mostrar alias, proveedor y cwd; ajustar el layout ante resize y terminales pequeñas.
- [ ] Implementar modos `NORMAL` e `INSERT` y un modal de creación de sesión.
- [ ] Conservar borrador y posición de scroll al alternar sesiones. Seguir la salida nueva solo si el usuario está al final del historial.
- [ ] Mostrar errores y cancelaciones en la conversación; mantener accesible el input para recuperación.
- [ ] Manejar raw mode y pantalla alternativa mediante una guarda de restauración, incluso ante inicialización parcial, errores y panic. Apagar procesos mediante la ruta común de cierre.

Atajos del MVP:

| Tecla | Acción |
| --- | --- |
| `i` o `Enter` en NORMAL | Entrar a INSERT |
| `Enter` en INSERT | Enviar turno; conservar el borrador si la sesión devuelve `busy` |
| `Ctrl+j` en INSERT | Insertar salto de línea sin depender de Shift+Enter |
| `Esc` en INSERT | Volver a NORMAL |
| `Esc` en NORMAL | Cancelar el turno activo, si existe |
| `j` / `k` o flechas en NORMAL | Cambiar sesión |
| `Ctrl+d` / `Ctrl+u` en NORMAL | Desplazar historial |
| `Ctrl+n` | Crear sesión: elegir Codex/agy, modelo opcional, alias y cwd; inicialmente el cwd de arranque |
| `q` en NORMAL | Salir y apagar todas las sesiones |

En el modal, `Tab` cambia de campo, `Enter` confirma y `Esc` cierra. Un cwd inválido mantiene abierto el modal con el error y los valores ingresados.

**Aceptación:** crear dos sesiones con distintos cwd, enviar y reanudar turnos, alternar conservando borradores/scroll y cancelar una generación, todo sin mouse. La otra sesión sigue operativa. Verificar resize, salida de orch y fallo de inicialización parcial: no quedan procesos controlados ni modos de terminal activos. La TUI nunca imprime logs en pantalla.

**Punto de revisión:** usar este flujo en repositorios reales y ajustar teclas, legibilidad y estados antes de sumar infraestructura. No es necesario completar todo el roadmap para empezar a usar orch.

## 4. Roadmap posterior

Estas etapas preservan la visión original; se detallarán al terminar la TUI mínima. No crear sus módulos o dependencias por anticipado.

### Fase 4 — Persistencia, configuración y Claude

- Persistir historial/eventos normalizados y metadatos bajo `.orch/sessions/` en el cwd de arranque, no en cada proyecto de las sesiones. Definir versión del formato y recuperación de una última línea incompleta.
- Al reiniciar, las sesiones que estaban ejecutando pasan a estado interrumpido; no se relanzan tareas automáticamente. Persistir el ID del proveedor no garantiza que el proveedor conserve la conversación: mostrar errores de reanudación.
- Agregar `~/.config/orch/config.toml` para proveedor, políticas verificadas, timeout y rutas. Los flags extra no pueden invalidar silenciosamente la política de permisos elegida.
- Integrar Claude con fixtures y pruebas de lanzamiento/reanudación. Verificar `--include-partial-messages` si se necesitan deltas; ampliar el contrato de runners solo cuando sus diferencias reales lo requieran.
- Cada proveedor debe superar las mismas pruebas de continuidad, cwd, errores, permisos y cancelación.
- Agregar rama y `git status --short` por cwd sin bloquear la UI. Documentar instalación con `cargo install --path .`.

### Fase 5 — Contexto entre sesiones y MCP

Primero incorporar `@alias`: autocompletado e inserción de una copia acotada del último resultado completado, con origen visible. Definir límite de tamaño y comportamiento si todavía no hay resultado. Luego habilitar acceso de agentes a otras sesiones por MCP.

- Elegir un SDK MCP mantenido y fijar una versión de protocolo compatible. Implementar inicialización, negociación de capacidades y ciclo de vida además de `tools/list` y `tools/call`.
- Usar Streamable HTTP en `127.0.0.1:0`, con validación de Origin y autenticación local. Asociar cada cliente a su sesión mediante credenciales emitidas por orch; no confiar en un identificador de origen declarado por el agente.
- Empezar con `list_sessions` y `get_session_context`, con límites de tamaño y alcance explícito entre proyectos.
- Agregar `ask_session(target, question)` sobre los controladores existentes. El servidor controla origen, cadena y profundidad; `hop_count` no es un parámetro confiable del modelo.
- Rechazar autoconsultas, destinos ocupados y ciclos; limitar inicialmente la cadena a dos saltos. Propagar su contexto desde orch a los turnos derivados.
- Aplicar timeout y cancelación a las consultas. Una consulta no mantiene locks mientras espera; solo cancela el turno destino que ella creó, nunca uno ajeno.
- Leer `~/.orch/shared/mcp.json` antes de cada lanzamiento/reanudación y fusionarlo con el servidor interno sin modificar el registro. Ausente equivale a vacío; JSON inválido o colisión con el nombre reservado `orch` impiden el lanzamiento con un error claro.
- Verificar la inyección MCP por proveedor, incluyendo reanudaciones. Cuando esta integración exista, los smoke tests también deben cubrirla.

**Aceptación a desarrollar:** A consulta a B sin intervención humana; se rechazan B ocupado, A→A y A→B→A; se cumple el límite de profundidad aunque el agente intente reiniciarlo. Timeout y cancelación liberan los turnos asociados. Probar autenticación, inicialización y configuración fusionada con registro ausente, inválido y colisiones.

Referencia de transporte y ciclo de vida: [especificación MCP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports).

### Fase 6 — Memoria y skills compartidas

Inicializar idempotentemente `~/.orch/shared/` con `mcp.json`, `memory/` y `skills/`, preservando datos existentes.

- Markdown como fuente de verdad para memoria; SQLite FTS5 como índice reconstruible. Serializar escrituras, reemplazar documentos de forma atómica y definir reparación tras fallo entre documento e índice, incluyendo acceso desde varias instancias de orch.
- Herramientas MCP `memory_search(query)` y `memory_write(topic, content)`, con resultados acotados, referencias y confinamiento de rutas que contemple symlinks.
- Herramientas `list_skills()` y `load_skill(name)` para archivos `SKILL.md`, con validación y confinamiento equivalentes.
- Documentar que la memoria es global entre proyectos y que las consultas pueden divulgar ese contenido a los proveedores utilizados.
- Sincronización opcional de symlinks según convenciones verificadas de cada proveedor: desactivada por defecto, sin sobrescribir archivos y con conflictos visibles. El acceso MCP funciona sin enlaces.

**Aceptación a desarrollar:** escritura desde un proyecto y búsqueda desde otro tras reiniciar; índice consistente o reparable ante concurrencia/fallo; lectura de skills por cada proveedor sin symlinks; enlaces opcionales preservan archivos existentes.

### Fase 7 — Mejoras de interacción y visuales

- `Ctrl+k`: command palette fuzzy para crear/cerrar sesiones, exportar Markdown y limpiar historial, con semántica explícita respecto de persistencia y conversación del proveedor.
- Kitty Keyboard Protocol con detección y fallback: Shift+Enter para salto de línea cuando pueda distinguirse; mantener `Ctrl+j` como alternativa. Distinguir y reservar Ctrl+Enter sin imponer todavía una acción configurable.
- Balancear `PushKeyboardEnhancementFlags`/`PopKeyboardEnhancementFlags` incluso ante errores y mantener el arranque operativo si la detección falla.
- Mejorar bloques de código y presentación según el uso real.
- Imágenes opcionales con `ratatui-image`: detectar gráficos Kitty por separado del protocolo de teclado, respetar scroll/layout y mostrar referencia textual cuando no haya soporte.

## 5. Reglas de ejecución y verificación

Las reglas permanentes y los comandos de calidad están en [AGENTS.md](AGENTS.md). Umbral automatizado: 90% de cobertura de líneas de producción, además de pruebas explícitas para todos los flujos críticos. Estado tras 1C: 18 tests offline, 95,77% de líneas cubiertas (2026-11-27); ver [CI](.github/workflows/ci.yml). La medición excluye solo el código de tests, no módulos de producción.


- Completar y verificar cada fase inicial antes de avanzar; el roadmap posterior puede reordenarse según uso real. Actualizar este archivo con decisiones y resultados concretos.
- 1A Codex, 1A-agy, 1B y 1C completadas. La fase 2 corresponde al controlador concurrente.
- No usar `unwrap()` en producción. Errores del proveedor, eventos nuevos y stdout inválido no deben causar panic ni éxito silencioso.
- Nunca usar `println!`/`eprintln!` con la TUI activa; usar tracing en archivo. La CLI headless sí puede imprimir eventos normalizados.
- Dejar pruebas pequeñas y deterministas para parsing, transiciones y ciclo de vida de procesos. Usar mocks/procesos controlados donde permitan reproducir fallos; evitar suites que solo repitan la implementación.
- Ejecutar `cargo check` y `cargo test` en cada fase de código. Separar claramente tests offline de smoke tests reales, que requieren autenticación y pueden consumir cuota.
- Documentar qué se verificó realmente y qué quedó pendiente por entorno, versión o acceso al proveedor. Una fase no queda validada por compilar solamente.
