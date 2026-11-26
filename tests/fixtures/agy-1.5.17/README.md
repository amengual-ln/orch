# Protocolo observado — agy 1.5.17 con Gemini

Capturas reales del 26 de noviembre de 2026, zona America/Argentina/Buenos_Aires, en Linux x86_64. Modelo fijado: `gemini-3.8-flash-low`, disponible en `agy models` para la cuenta utilizada. No se cambió de modelo silenciosamente ni se usó el default del CLI.

## Pruebas y archivos

| Captura | Resultado observado |
| --- | --- |
| `turn-1.jsonl` | Primer proceso: recordar `ORCH-AGY-1A-9B2D`, responder `OK`; `SUCCESS`, exit code 0. |
| `turn-2.jsonl` | Nuevo proceso con `--conversation <id>`: recupera el marcador sin recibirlo otra vez; mismo ID, `num_turns: 2`, `SUCCESS`, exit code 0. |
| `stream.jsonl` y `stream.stdin.jsonl` | Un proceso, dos prompts enviados con flush; el segundo se envió solo después del primer `result`. Recupera `ORCH-STREAM-4E6A`; ambos resultados son `SUCCESS`. El proceso seguía vivo después de cada resultado y terminó con exit code 0 tras cerrar stdin. |
| `invalid-model.jsonl` | Modelo inexistente: un `result` con `ERROR`, ID vacío y sin `init`; exit code 1 y uso de tokens cero. |
| `invalid-input.jsonl` | Bloque de entrada `image` no admitido: `init` seguido de `result` con `ERROR`; exit code 1 y uso de tokens cero. La entrada exacta está en los metadatos. |
| `permission-denied.jsonl` | Se solicita únicamente `run_command` con `printf ORCH_PERMISSION_PROBE`; la herramienta se deniega con `strict`. Hay `state: ERROR` y `tool_info.error`, pero el resultado final declara `SUCCESS`, respuesta vacía y `denied_actions`; exit code 0. |
| `plan-disabled.jsonl` | Prueba inicial conservada por separado: respondió `OK`, pero stderr advirtió que `--disable-slash-commands` anulaba `--mode plan`. Las capturas principales evitan esa combinación. |

Cada captura tiene su archivo `*.stderr.txt`, incluso si está vacío. `metadata.json` contiene argumentos, stdin, códigos de salida, timeout externo y observaciones del harness. `help.txt` y `version.txt` vienen del ejecutable instalado; en esta versión la ayuda se imprime en stderr. `models-gemini-excerpt.txt` transcribe las tres variantes Flash devueltas por `agy models`; es un extracto, no una lista exhaustiva ni una garantía de disponibilidad futura.

Se reemplazaron consistentemente los UUID de conversación por UUID ficticios y el cwd temporal por `/tmp/orch-agy-fixture/cwd`; se reserializó JSON sin cambiar el resto de los campos. Los IDs de estas fixtures no permiten reanudar conversaciones reales. Los prompts son sintéticos y no se guardaron credenciales ni archivos de sesión.

El entorno restringido bloqueó logs y apertura del servidor TCP local de `agy models`. Las capturas se obtuvieron con ejecución en el host autorizada. Se solicitó `--sandbox`; eso no prueba por sí solo su aislamiento efectivo. La consulta de modelos y las conversaciones usaron la autenticación existente, sin modificar manualmente la configuración del usuario.

## Invocaciones verificadas

Ejecutar desde el directorio de trabajo de la sesión, inicialmente vacío en estas pruebas:

```sh
agy --model gemini-3.8-flash-low \
  --mode plan --sandbox --settings '{"toolPermission":"strict"}' \
  --output-format stream-json --print-timeout 45s \
  --print 'No uses herramientas ni leas archivos. Recordá el marcador ORCH-AGY-1A-9B2D para el próximo turno de esta conversación. Respondé únicamente OK.'
```

Reanudar desde el mismo cwd y con el ID real recibido:

```sh
agy --model gemini-3.8-flash-low \
  --mode plan --sandbox --settings '{"toolPermission":"strict"}' \
  --output-format stream-json --print-timeout 45s \
  --conversation UUID_REAL_DEL_PRIMER_TURNO \
  --print 'Sin usar herramientas ni leer archivos: ¿cuál era el marcador que te pedí recordar en el turno anterior? Respondé únicamente con ese marcador.'
```

Para un proceso vivo, sustituir `--print <prompt>` por `--input-format stream-json` y escribir líneas como esta en stdin:

```json
{"event":"user","message":{"content":"No uses herramientas. Respondé únicamente OK."}}
```

Hacer flush, leer hasta el `result` del turno y enviar el siguiente prompt. Cerrar stdin al terminar. La prueba mantuvo stdin abierto después de cada resultado y comprobó que el proceso seguía vivo; no confundió un resultado con EOF. `stream.stdin.jsonl` conserva las dos entradas exactas, pero enviarlas juntas por un pipe no reproduce esa comprobación temporal.

## Contrato observado para el parser

El discriminante es `event`, no `type` como en Codex:

| Evento/campo | Interpretación |
| --- | --- |
| `conversation_id` | En el evento `init`, el ID está en la raíz, no en `init.conversation_id`; en actualizaciones/resultados está dentro de su payload. |
| `init` | Una inicialización por proceso en estas capturas, también al reanudar. `init.model`, `init.cwd` e `init.permission_mode` permiten verificar modelo solicitado, cwd declarado y `strict`. Incluye herramientas disponibles, no herramientas ejecutadas. |
| `step_update` | El payload tiene `conversation_id`, `step_index`, `state` y `step_type`. Se observaron `user_input`, `system_message`, `agent_response` y `tool`. No hay un evento explícito equivalente a `turn.started`; orch conoce el inicio por el envío aceptado. |
| `step_update.text_delta` | Fragmento incremental; puede aparecer con `ACTIVE` y con `DONE`. En `turn-2`, el último `DONE` aporta solo `\n`, no la respuesta completa. Concatenar por paso, sin reemplazar los fragmentos previos por el último evento. |
| `step_update.tool_info` | En la prueba de permisos trae nombre/parámetros en `ACTIVE`, y `error.type`/`error.message` en `ERROR`. Un intento no equivale a herramienta ejecutada. |
| `result` | El payload contiene `conversation_id`, `status`, `response`, `duration_seconds`, `num_turns` y `usage`; puede incluir `error`, `finished_reason` y `denied_actions`. Resultado de turno, no necesariamente cierre de proceso. |

`result.response` es el texto consolidado: no agregarlo como otro mensaje después de sus deltas. En proceso vivo, el `num_turns` observado pasa de 1 a 2; al reanudar otro proceso también vale 2. No asumir que `usage` o `duration_seconds` son mediciones exclusivas del último turno: en las capturas incluyen acumulación. Conservarlos como métricas crudas del proveedor hasta definir su presentación.

El parser debe aceptar errores con `conversation_id: ""` y sin inicialización previa. `finished_reason: error` apareció en los dos errores controlados; no exigir ese campo para todos los resultados. Conservar tolerancia a campos desconocidos.

## Permisos y terminación

Las pruebas principales solicitaron `--mode plan --sandbox --settings '{"toolPermission":"strict"}'`; los `init` confirman `strict`. En invocaciones `--print`, `expanded_commands` incluye `plan`; ese campo no apareció en la captura por stdin. No usar el modo plan como garantía de confinamiento ni atribuirle un efecto que no se verificó.

La prueba de rechazo confirmó que el comando solicitado no se ejecutó y que el agente no buscó otra vía. El aviso en stderr propone habilitar permisos o bypass: es diagnóstico del proveedor, no una instrucción que orch deba ejecutar. No se habilitó ninguna excepción.

Para el MVP, normalizar resultados con `denied_actions` como fallo de permisos conservando respuesta/diagnóstico, aunque el CLI salga con 0 y `status: SUCCESS`. La UI debe distinguir este caso de una generación completada sin rechazos. Todavía falta probar escrituras, recuperación tras cancelación, timeout y limpieza del servidor/descendientes.

La documentación oficial explica que headless puede denegar herramientas y continuar con exit code 0. También aclara que el sandbox de terminal puede no estar disponible en algunas máquinas: `--sandbox` no equivale al `--sandbox read-only` de Codex. [Modo headless](https://antigravity.google/docs/cli/headless/), [referencia CLI](https://antigravity.google/docs/cli/reference/).

## Decisión para la primera implementación

Usar un proceso por turno con reanudación explícita, igualando inicialmente el ciclo de vida con Codex. El proceso vivo ya está validado como capacidad del proveedor, pero se deja para una optimización posterior. El modelo es una propiedad de la sesión y se repite al reanudar; el ejecutable/protocolo es `agy` y el modelo probado es Gemini.

Estas capturas se verificaron con Python estándar: JSONL válido, mismo ID al reanudar, respuestas esperadas, concatenación de deltas, dos resultados en el stream, proceso vivo entre resultados y códigos de salida. No hay parser Rust ni proyecto Cargo todavía.
