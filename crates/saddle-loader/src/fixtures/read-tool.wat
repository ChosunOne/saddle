(component
  (type $host (instance
    (type $path-record (record (field "root" string) (field "relative" string)))
    (export "path" (type $path (eq $path-record)))
    (type $failure-variant (variant (case "denied" string) (case "invalid-path" string)
      (case "io" string) (case "too-large")))
    (export "failure" (type $failure (eq $failure-variant)))
    (type $result (result (list u8) (error $failure)))
    (export "read-file" (func (param "path" $path) (result $result)))
  ))
  (import "saddle:host/fs-read@0.1.0" (instance $host (type $host)))
  (alias export $host "read-file" (func $read))

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
  (core func $read-lowered (canon lower (func $read)
    (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))

  (core module $guest
    (import "memory" "memory" (memory 1))
    (import "host" "read-file" (func $read (param i32 i32 i32 i32 i32)))
    ;; initialization-probe
    (global $calls (mut i32) (i32.const 0))
    (data (i32.const 128) "reader")
    (data (i32.const 144) "read one")
    (data (i32.const 160) "{}")
    (data (i32.const 256) "workspace")
    (data (i32.const 272) "denied")
    (func (export "describe") (result i32)
      ;; describe-probe
      i32.const 0 i32.const 128 i32.store
      i32.const 4 i32.const 6 i32.store
      i32.const 8 i32.const 144 i32.store
      i32.const 12 i32.const 8 i32.store
      i32.const 16 i32.const 160 i32.store
      i32.const 20 i32.const 2 i32.store
      i32.const 0)
    (func (export "invoke") (param $path i32) (param $len i32) (result i32)
      ;; invoke-probe
      global.get $calls if unreachable end
      i32.const 1 global.set $calls
      i32.const 256 i32.const 9 local.get $path local.get $len i32.const 32 call $read
      i32.const 32 i32.load8_u
      if
        i32.const 64 i32.const 272 i32.store
        i32.const 68 i32.const 6 i32.store
        i32.const 72 i32.const 1 i32.store8
      else
        i32.const 64 i32.const 36 i32.load i32.store
        i32.const 68 i32.const 40 i32.load i32.store
        i32.const 72 i32.const 0 i32.store8
      end
      i32.const 64)
  )
  (core instance $guest (instantiate $guest
    (with "memory" (instance $memory))
    (with "host" (instance (export "read-file" (func $read-lowered))))))

  (type $definition (record (field "name" string) (field "description" string)
    (field "input-schema" string)))
  (type $output (record (field "text" string) (field "is-error" bool)))
  (func $describe (result $definition) (canon lift (core func $guest "describe")
    (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))
  (func $invoke (param "arguments" string) (result $output)
    (canon lift (core func $guest "invoke")
      (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))
  (instance $tool
    (export "definition" (type $definition))
    (export "output" (type $output))
    (export "describe" (func $describe))
    (export "invoke" (func $invoke)))
  (export "saddle:plugin/tool@0.1.0" (instance $tool))
)
