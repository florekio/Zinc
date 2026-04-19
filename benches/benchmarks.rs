use criterion::{black_box, criterion_group, criterion_main, Criterion};
use zinc::engine::Engine;

const FIB_SRC: &str = include_str!("../bench/fib.js");
const LOOP_SUM_SRC: &str = include_str!("../bench/loop_sum.js");
const STRING_CONCAT_SRC: &str = include_str!("../bench/string_concat.js");
const CLOSURE_COUNTER_SRC: &str = include_str!("../bench/closure_counter.js");
const OBJECT_CREATE_SRC: &str = include_str!("../bench/object_create.js");
const SIEVE_SRC: &str = include_str!("../bench/sieve.js");
const NBODY_SRC: &str = include_str!("../bench/sunspider/access-nbody.js");

fn bench_script(c: &mut Criterion, name: &str, src: &str) {
    let mut group = c.benchmark_group(name);
    group.bench_function("execute", |b| {
        b.iter_with_setup(
            Engine::new,
            |mut engine| {
                let _ = engine.eval(black_box(src));
            },
        )
    });
    group.finish();
}

fn bench_fib(c: &mut Criterion) {
    bench_script(c, "fib", FIB_SRC);
}

fn bench_loop_sum(c: &mut Criterion) {
    bench_script(c, "loop_sum", LOOP_SUM_SRC);
}

fn bench_string_concat(c: &mut Criterion) {
    bench_script(c, "string_concat", STRING_CONCAT_SRC);
}

fn bench_closure_counter(c: &mut Criterion) {
    bench_script(c, "closure_counter", CLOSURE_COUNTER_SRC);
}

fn bench_object_create(c: &mut Criterion) {
    bench_script(c, "object_create", OBJECT_CREATE_SRC);
}

fn bench_sieve(c: &mut Criterion) {
    bench_script(c, "sieve", SIEVE_SRC);
}

fn bench_nbody(c: &mut Criterion) {
    bench_script(c, "nbody", NBODY_SRC);
}

criterion_group!(
    benches,
    bench_fib,
    bench_loop_sum,
    bench_string_concat,
    bench_closure_counter,
    bench_object_create,
    bench_sieve,
    bench_nbody,
);
criterion_main!(benches);
