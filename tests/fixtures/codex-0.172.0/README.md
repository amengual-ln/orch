# Protocolo observado — Codex CLI 0.172.0

Capturas reales de la fase 1A en Linux, el 25 de noviembre de 2026 (America/Argentina/Buenos_Aires). No son eventos simulados ni un esquema exhaustivo del proveedor.

## Archivos y procedencia

- `turn-1.jsonl` y `turn-2.jsonl`: stdout, un objeto JSON por línea. Se reemplazó únicamente el `thread_id` por un UUID estable y se reserializó el JSON. El ID de estas fixtures no sirve para reanudar la conversación real.
- `turn-1.stderr.txt` y `turn-2.stderr.txt`: stderr separado; el segundo está vacío.
- `metadata.json`: prompts exactos, argumentos con placeholders para ruta/ID reales, códigos de salida, condiciones y límites de la prueba. El prompt del segundo turno no contiene el marcador.
- `cli-help.txt`, `exec-help.txt`, `resume-help.txt`: stdout de la ayuda instalada, con espacios finales normalizados y sin la advertencia de escritura que aparecía en stderr del entorno restringido.
- `startup-readonly.stderr.txt`: fallo real previo a los eventos por estado local no escribible; stdout vacío y exit code 1.
- `resume-invalid-option.stderr.txt`: error CLI provocado al colocar `--sandbox` después de `resume`; stdout vacío y exit code 2. Incluye una advertencia del entorno además del error de argumentos. No llamó al modelo.

Los prompts son sintéticos y el agente no ejecutó herramientas ni leyó archivos del proyecto. No se guardaron credenciales, archivos de sesión del proveedor ni rutas reales en los metadatos publicados.

## Secuencia de éxito observada

Ambos procesos emitieron esta secuencia, y después terminaron con exit code 0:

| Evento | Campos observados | Interpretación para el runner |
| --- | --- | --- |
| `thread.started` | `thread_id` | Identifica la conversación; también aparece al reanudar y conserva su ID. No significa necesariamente conversación nueva. |
| `turn.started` | `type` | Inicio del turno; no trae un ID de turno en estas capturas. Orch debe correlacionarlo con el turno que lanzó. |
| `item.completed` | `item.id`, `item.type: agent_message`, `item.text` | Mensaje completo. Su texto fue `OK` y luego el marcador; no tomarlo como fin de turno. |
| `turn.completed` | `usage` | Fin satisfactorio del protocolo observado; combinar con la salida correcta del proceso para confirmar éxito. No contiene `final_text`. |

`item.id` vale `item_0` en ambos turnos: no asumir que identifica mensajes globalmente. Para estos casos, obtener el texto final de los mensajes emitidos antes de `turn.completed`. Varias respuestas, deltas y herramientas requieren muestras adicionales antes de definir su consolidación.

`usage` incluye `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens` y `reasoning_output_tokens`. Conservar campos adicionales sin exigir un conjunto cerrado. No se observó `TextDelta`; no inventar deltas a partir de un mensaje completo.

En el primer turno stderr contiene `Reading additional input from stdin...`, aunque stdin estaba cerrado y el prompt venía por argumento. El proceso fue satisfactorio: stderr no vacío por sí solo no equivale a fallo.

## Errores y límites

Los errores capturados ocurrieron antes de iniciar un turno JSONL. Por lo tanto, el runner debe poder fallar con stdout vacío usando exit code y diagnóstico de stderr, sin esperar obligatoriamente un evento de error.

Todavía **no se verificaron** los nombres y campos de eventos de fallo durante un turno, actualizaciones de herramientas ni deltas. No deducir un contrato de error del proveedor a partir de estos errores de inicialización/argumentos. En 1B, agregar casos deterministas de protocolo incompleto, eventos desconocidos y líneas inválidas, identificándolos como sintéticos; incorporar capturas adicionales del proveedor cuando se obtengan.

La verificación de 1A parseó ambas capturas, comprobó la misma conversación, los textos esperados, el orden de eventos y los exit codes reales. Todavía no existe parser Rust ni suite Cargo. Los eventos sin `session_id`/`turn_id` deben envolverse con los IDs internos de orch al implementar el runner.
