(component
  (import "write-text" (func $write (param "name" string) (param "text" string) (result (result string (error string)))))
  (core module $memory
    (memory (export "memory") 1)
    (data (i32.const 512) "plugin-result")
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 2048))
  (core instance $mem (instantiate $memory))
  (core func $write-core (canon lower (func $write)
    (memory (core memory $mem "memory")) (realloc (core func $mem "realloc"))))
  (core module $wrapper
    (import "host" "write" (func $write (param i32 i32 i32 i32 i32)))
    (func (export "run") (param i32 i32) (result i32)
      i32.const 512 i32.const 13 local.get 0 local.get 1 i32.const 16 call $write i32.const 16))
  (core instance $wrapper-instance (instantiate $wrapper
    (with "host" (instance (export "write" (func $write-core))))))
  (func (export "run") (param "input" string) (result (result string (error string)))
    (canon lift (core func $wrapper-instance "run")
      (memory (core memory $mem "memory")) (realloc (core func $mem "realloc")))))
