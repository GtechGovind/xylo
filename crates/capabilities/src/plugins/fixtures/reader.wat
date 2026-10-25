(component
  (import "read-text" (func $read (param "path" string) (result (result string (error string)))))
  (core module $memory
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024))
  (core instance $mem (instantiate $memory))
  (core func $read-core (canon lower (func $read)
    (memory (core memory $mem "memory")) (realloc (core func $mem "realloc"))))
  (core module $wrapper
    (import "host" "read" (func $read (param i32 i32 i32)))
    (func (export "run") (param i32 i32) (result i32)
      local.get 0 local.get 1 i32.const 16 call $read i32.const 16))
  (core instance $wrapper-instance (instantiate $wrapper
    (with "host" (instance (export "read" (func $read-core))))))
  (func (export "run") (param "input" string) (result (result string (error string)))
    (canon lift (core func $wrapper-instance "run")
      (memory (core memory $mem "memory")) (realloc (core func $mem "realloc")))))
