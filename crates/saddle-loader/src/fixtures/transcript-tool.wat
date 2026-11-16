(component
  (type $host (instance
    (type $result (result string (error string)))
    (export "query" (func (param "sql" string) (param "session" string) (result $result)))
  ))
  (import "saddle:host/transcript-read@0.1.0" (instance $host (type $host)))
  (alias export $host "query" (func $query))
  (core module $memory
    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 4096))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $out i32)
      global.get $bump local.tee $out
      local.get 3 i32.add i32.const 7 i32.add i32.const -8 i32.and
      global.set $bump local.get $out)
  )
  (core instance $memory (instantiate $memory))
  (core func $query-lowered (canon lower (func $query)
    (memory $memory "memory") (realloc (func $memory "realloc"))))
  (core module $guest
    (import "memory" "memory" (memory 1))
    (import "host" "query" (func $query (param i32 i32 i32 i32 i32)))
    ;; initialization-probe
    (data (i32.const 128) "session_query")
    (data (i32.const 144) "query branch")
    (data (i32.const 160) "{}")
    (data (i32.const 176) "s")
    (func (export "describe") (result i32)
      i32.const 0 i32.const 128 i32.store
      i32.const 4 i32.const 13 i32.store
      i32.const 8 i32.const 144 i32.store
      i32.const 12 i32.const 12 i32.store
      i32.const 16 i32.const 160 i32.store
      i32.const 20 i32.const 2 i32.store
      i32.const 0)
    (func (export "invoke") (param $sql i32) (param $len i32) (result i32)
      ;; query-probe
      ;; the session is the empty string: the conversation in hand
      local.get $sql local.get $len i32.const 176 i32.const 0 i32.const 32 call $query
      i32.const 64 i32.const 36 i32.load i32.store
      i32.const 68 i32.const 40 i32.load i32.store
      i32.const 72 i32.const 32 i32.load8_u i32.store8
      i32.const 64)
  )
  (core instance $guest (instantiate $guest
    (with "memory" (instance $memory))
    (with "host" (instance (export "query" (func $query-lowered))))))
  (type $definition (record (field "name" string) (field "description" string)
    (field "input-schema" string)))
  (type $output (record (field "text" string) (field "is-error" bool)))
  (func $describe (result $definition) (canon lift (core func $guest "describe")
    (memory $memory "memory") (realloc (func $memory "realloc"))))
  (func $invoke (param "arguments" string) (result $output)
    (canon lift (core func $guest "invoke")
      (memory $memory "memory") (realloc (func $memory "realloc"))))
  (instance $tool
    (export "definition" (type $definition))
    (export "output" (type $output))
    (export "describe" (func $describe))
    (export "invoke" (func $invoke)))
  (export "saddle:plugin/tool@0.1.0" (instance $tool))
)
