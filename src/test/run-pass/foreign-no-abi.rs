// ABI is cdecl by default

extern mod rustrt {
    fn get_task_id() -> int;
}

fn main() {
    rustrt::get_task_id();
}
