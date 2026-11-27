(component
  (type $host (instance
    (type $expected-type (variant (case "any") (case "missing") (case "sha256" string)))
    (export "expectation" (type $expected (eq $expected-type)))
    (type $change-type (record (field "path" string) (field "content" (option (list u8))) (field "expected" $expected)))
    (export "change" (type $change (eq $change-type)))
    (type $result (result (error string)))
    (export "apply" (func (param "changes" (list $change)) (result $result)))
  ))
  (import "saddle:host/fs-write@0.1.0" (instance $host (type $host)))
  (alias export $host "apply" (func $apply))
  (core module $memory
    (memory (export "memory") 32)
    (global $bump (mut i32) (i32.const 1500000))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $out i32)
      global.get $bump local.tee $out
      local.get 3 i32.add i32.const 7 i32.add i32.const -8 i32.and
      global.set $bump local.get $out)
  )
  (core instance $memory (instantiate $memory))
  (core func $apply-lowered (canon lower (func $apply)
    (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))
  (core module $guest
    (import "memory" "memory" (memory 32))
    (import "host" "apply" (func $apply (param i32 i32 i32)))
    ;; mutation-data
    (data (i32.const 128) "mutator")
    (data (i32.const 144) "mutate files")
    (data (i32.const 160) "{}")
    (data (i32.const 180) "committed")
    (func $mutate
      ;; mutation-setup
      i32.const 256 i32.const 1 i32.const 32 call $apply)
    (func (export "describe") (result i32)
      ;; describe-probe
      i32.const 0 i32.const 128 i32.store
      i32.const 4 i32.const 7 i32.store
      i32.const 8 i32.const 144 i32.store
      i32.const 12 i32.const 12 i32.store
      i32.const 16 i32.const 160 i32.store
      i32.const 20 i32.const 2 i32.store
      i32.const 0)
    (func (export "invoke") (param i32 i32) (result i32)
      call $mutate
      ;; after-mutation
      i32.const 32 i32.load8_u
      if
        i32.const 64 i32.const 36 i32.load i32.store
        i32.const 68 i32.const 40 i32.load i32.store
        i32.const 72 i32.const 1 i32.store8
      else
        i32.const 64 i32.const 180 i32.store
        i32.const 68 i32.const 9 i32.store
        i32.const 72 i32.const 0 i32.store8
      end
      i32.const 64)
  )
  (core instance $guest (instantiate $guest
    (with "memory" (instance $memory))
    (with "host" (instance (export "apply" (func $apply-lowered))))))
  (type $definition (record (field "name" string) (field "description" string) (field "input-schema" string)))
  (type $output (record (field "text" string) (field "is-error" bool)))
  (func $describe (result $definition) (canon lift (core func $guest "describe")
    (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))
  (func $invoke (param "arguments" string) (result $output) (canon lift (core func $guest "invoke")
    (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))
  (instance $tool
    (export "definition" (type $definition)) (export "output" (type $output))
    (export "describe" (func $describe)) (export "invoke" (func $invoke)))
  (export "saddle:plugin/tool@0.1.0" (instance $tool))
)
