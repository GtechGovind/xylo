(component
  (core module $module
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024)
    (func (export "run") (param $p i32) (param $n i32) (result i32)
      i32.const 16 i32.const 0 i32.store
      i32.const 20 local.get $p i32.store
      i32.const 24 local.get $n i32.store
      i32.const 16))
  (core instance $instance (instantiate $module))
  (func (export "run") (param "input" string) (result (result string (error string)))
    (canon lift (core func $instance "run")
      (memory (core memory $instance "memory"))
      (realloc (core func $instance "realloc")))))
