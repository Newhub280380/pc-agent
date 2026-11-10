//! Короткий замер: кэш решений и успешность отката.
//! Печатает числа, которые попадают в отчёт. Это тест, а не пример:
//! откат обязан пройти все циклы, а QUBO — попасть в тот же оптимум,
//! что и точный перебор.

#[cfg(test)]
mod tests {
    use crate::checkpoint::{read_files, Snapshot, Store};
    use crate::decisions::Client;
    use crate::memory::Memory;
    use crate::optimize::{self, Annealer, Kind};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn bench_decision_latency_and_recovery() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let mut client = Client::with_transport(
            "http://jev.local/decisions",
            Some("bench-key".into()),
            Box::new(move |_url, _bearer, _body| {
                calls2.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
                Ok(json!({"answers": {"q": {"type": "noul", "noul": 0.88}}}))
            }),
        );
        let state = json!({"action": "клик", "goal": "открыть"});
        let cold_at = Instant::now();
        let first = client.noul(&state, "Делать ли клик?").unwrap();
        let cold = cold_at.elapsed();
        let warm_at = Instant::now();
        let rounds = 200u32;
        for _ in 0..rounds {
            let again = client.noul(&state, "Делать ли клик?").unwrap();
            assert_eq!(again, first);
        }
        let warm = warm_at.elapsed();
        let per_hit_ns = warm.as_nanos() / u128::from(rounds);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            per_hit_ns < cold.as_nanos(),
            "повтор из кэша ({per_hit_ns} нс) должен быть быстрее первого вызова ({} нс)",
            cold.as_nanos()
        );

        let dir = std::env::temp_dir().join(format!(
            "pcagent-bench-{}-{}",
            std::process::id(),
            crate::watchdog::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("ckpt")).unwrap();
        let cycles = 20;
        let mut ok = 0;
        let recover_at = Instant::now();
        for i in 0..cycles {
            let file = dir.join(format!("f{i}.txt"));
            std::fs::write(&file, format!("v{i}")).unwrap();
            let db = dir.join(format!("m{i}.db"));
            let mem = Memory::open(&db).unwrap();
            mem.remember("app:bench", "fact", "row", "before", "", 0.5)
                .unwrap();
            let mut graph = crate::agent::graph::TaskGraph::linear(
                "bench",
                &[("шаг".into(), "видно «Готово»".into())],
            );
            graph.validate().unwrap();
            let snap = Snapshot {
                task_id: format!("b{i}"),
                task: "bench".into(),
                step: 1,
                graph: graph.clone(),
                observed: BTreeMap::new(),
                window_id: 0,
                window_title: String::new(),
                files: read_files(&[file.clone()]).unwrap(),
                criteria: vec!["«Готово»".into()],
                trace: Vec::new(),
                touched: vec![file.display().to_string()],
            };
            store.save(&snap, &mem).unwrap();
            std::fs::write(&file, b"DIRTY").unwrap();
            mem.remember("app:bench", "fact", "row2", "after", "", 0.5)
                .unwrap();
            let mut mem = mem;
            let back = store
                .restore_latest(&format!("b{i}"), &mut mem, &mut |_, _| Ok(()))
                .unwrap();
            let same_file = std::fs::read(&file).unwrap() == format!("v{i}").into_bytes();
            let same_graph = back.graph.goal == graph.goal;
            let memory_clean = mem.recall("app:bench", "after", 5).unwrap().is_empty()
                && mem.recall("app:bench", "before", 5).unwrap().len() == 1;
            if same_file && same_graph && memory_clean {
                ok += 1;
            }
        }
        let recover = recover_at.elapsed();
        assert_eq!(ok, cycles, "откат должен проходить целиком в каждом цикле");

        let problem = optimize::Problem {
            jobs: vec![
                optimize::Job {
                    id: "a".into(),
                    deps: vec![],
                    variants: vec![
                        optimize::Cost {
                            id: "expensive".into(),
                            time: 2,
                            cost: 9,
                            risk: 5,
                        },
                        optimize::Cost {
                            id: "cheap".into(),
                            time: 1,
                            cost: 1,
                            risk: 1,
                        },
                    ],
                },
                optimize::Job {
                    id: "b".into(),
                    deps: vec!["a".into()],
                    variants: vec![optimize::Cost {
                        id: "base".into(),
                        time: 1,
                        cost: 1,
                        risk: 1,
                    }],
                },
            ],
            limits: optimize::Limits {
                max_time: 10,
                max_cost: 5,
                max_risk: 10,
            },
        };
        let exact = optimize::solve_exact(&problem).unwrap();
        let qubo_at = Instant::now();
        let qubo = optimize::solve_qubo(
            &problem,
            &Annealer {
                seed: 1,
                steps: 4000,
            },
        )
        .unwrap();
        let qubo_ms = qubo_at.elapsed().as_millis();
        assert_eq!(qubo.cost, exact.cost);

        let ext_at = Instant::now();
        let external = match optimize::solve(Kind::MiniZinc, &problem) {
            Ok(sol) => format!("{} cost={}", sol.solver, sol.cost),
            Err(e) => format!("unavailable: {}", crate::text::truncate(&e, 120)),
        };
        let ext_ms = ext_at.elapsed().as_millis();

        let line = format!(
            "decision_cold_us={} decision_cached_ns={} decision_calls={} recovery_success={}/{} recovery_ms={} qubo_cost={} exact_cost={} qubo_ms={} external_ms={} external={}",
            cold.as_micros(),
            per_hit_ns,
            calls.load(Ordering::SeqCst),
            ok,
            cycles,
            recover.as_millis(),
            qubo.cost,
            exact.cost,
            qubo_ms,
            ext_ms,
            external,
        );
        eprintln!("{line}");
        let _ = std::fs::write("/tmp/pcagent-stage-bench.txt", &line);
    }
}
