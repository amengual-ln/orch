# Reglas de trabajo para orch

## Alcance y diseño

- Leer `PLAN.md`, el código afectado y sus callers antes de implementar. Completar y verificar la fase actual antes de avanzar.
- Mantener la solución mínima que cubra el comportamiento requerido. Reutilizar el control de procesos y contratos existentes; no anticipar abstracciones de fases futuras.
- Plataforma inicial: Linux. Rust mínimo verificado 1.100, edición 2024. Mantener `Cargo.lock` para reproducibilidad.

## Git y pull requests

- Trabajar siempre en una rama de tarea y abrir PR contra `main`. No hacer push directo a `main`, force-push ni borrar la rama protegida.
- El merge requiere el check `quality` de GitHub Actions en verde y la rama actualizada con `main`; incluye formato, check, Clippy, tests y cobertura mínima del 90%.
- No se requieren aprobaciones de otras personas: el autor puede mergear su PR cuando CI pasa.
- Las reglas se aplican también al administrador, sin actores de bypass. No desactivar protecciones, reducir umbrales ni falsificar checks para permitir un merge.
- La definición versionada está en `.github/main-ruleset.json`; verificar su estado efectivo en GitHub al modificarla. Los cambios de configuración remota no se aplican por editar solamente ese archivo.

## Pruebas y regresiones

- Todo cambio de comportamiento debe incluir una prueba que falle si ese comportamiento se rompe. Para bugs, reproducir primero el fallo y conservar la prueba como regresión.
- Objetivo y umbral de CI: **al menos 90% de cobertura de líneas del código Rust de producción en `src/`**, medido con `cargo-llvm-cov`. No se exige 100%; la cifra no sustituye probar los caminos críticos.
- No excluir módulos de producción, desactivar tests, debilitar asserts ni añadir tests sin valor para alcanzar el porcentaje. Solo se excluye del cálculo el código de tests; cualquier otra excepción requiere justificación explícita y revisión.
- Cubrir todos los flujos críticos, tanto de éxito como de fallo: parsing y consolidación de mensajes; IDs y reanudación; cwd y argumentos por sesión; permisos denegados; concurrencia y `busy`; timeout/cancelación; señales, descendientes y recolección; exactamente un evento terminal por turno. Agregar persistencia y restauración de terminal cuando esas fases existan.
- Probar bordes relevantes: salida abundante/fragmentada/inválida, EOF incompleto, campos/eventos desconocidos, ejecutables ausentes, errores de proveedor y límites de memoria. Los procesos de prueba deben ser deterministas y tener limpieza y tiempos máximos.
- Los tests offline no usan autenticación, modelos, red ni cuota. Usar fixtures anonimizadas y procesos falsos; identificar muestras sintéticas como tales. Los smoke tests reales son explícitos y separados de CI.
- No asumir que un exit code 0 implica que la tarea se completó. Verificar también el contrato del proveedor y los rechazos de permisos.
- Correr tests en un Linux que permita señales y grupos de procesos. Si el entorno restringido interfiere, reproducir en el host y registrar la diferencia; no omitir el test ni declarar que pasó en un entorno donde falló.

## Checks antes de dar un cambio por completo

```sh
cargo fmt --all -- --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo llvm-cov --locked --all-targets --ignore-filename-regex '(^|/)tests/' --fail-under-lines 90
```

Instalación de la herramienta de cobertura (fuera de las dependencias del producto):

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked --version 0.9.2
```

- Informar qué se verificó, la cobertura medida y las limitaciones reales. Un check bloqueado o fallido no cuenta como aprobado.
- Si se cambia un runner o el ciclo de vida de procesos, ejecutar además el smoke test real correspondiente cuando haya acceso al proveedor; registrar si queda pendiente.

## Procesos, datos y logs

- Sin `unwrap()` ni `expect()` en producción. Manejar errores sin ocultar fallos ni pérdida de datos.
- Ejecutar programas con argumentos separados; no interpolar prompts o rutas en comandos de shell.
- No mantener locks al esperar procesos/proveedores. Mantener límites de memoria, canales y tiempos.
- No modificar globalmente el cwd, credenciales, políticas de permisos ni configuración del usuario para hacer pasar tests. No habilitar bypass de permisos.
- Logs operativos a `.orch/orch.log` desde el cwd de arranque. No guardar prompts, secretos ni líneas crudas del proveedor. Nada de `println!`/`eprintln!` con la TUI activa.
