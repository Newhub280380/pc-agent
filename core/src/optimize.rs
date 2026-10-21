//! Порядок шагов и выбор варианта под лимитами времени, цены и риска.
//!
//! Сначала MiniZinc через командную строку. Если её нет в PATH — OR-Tools
//! (`python3 -c` с `ortools.sat`). Оба вызываются отдельным процессом с
//! таймаутом. Нет ни того ни другого — ошибка, и вызывающий оставляет
//! порядок планировщика. Подменять их «примерно тем же» обходом нельзя:
//! Z3 всё равно заново доказывает план перед исполнением.
//!
//! Третий решатель — QUBO и имитация отжига. Он включается флагом
//! `PCAGENT_OPTIMIZER=qubo`. Отжиг реализует `QuboMinimizer`: тот же
//! интерфейс потом может занять реальный квантовый процессор, не трогая
//! постановку задачи.

use crate::agent::graph::TaskGraph;
use crate::sandbox::{self, Limits as RunLimits};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    MiniZinc,
    Qubo,
}

impl Kind {
    pub fn from_env() -> Self {
        match std::env::var("PCAGENT_OPTIMIZER")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "qubo" | "anneal" | "quantum" => Kind::Qubo,
            _ => Kind::MiniZinc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cost {
    pub id: String,
    pub time: u32,
    pub cost: u32,
    pub risk: u32,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub deps: Vec<String>,
    pub variants: Vec<Cost>,
}

#[derive(Debug, Clone)]
pub struct Problem {
    pub jobs: Vec<Job>,
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_time: u32,
    pub max_cost: u32,
    pub max_risk: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Solution {
    pub order: Vec<String>,
    pub variant: BTreeMap<String, String>,
    pub time: u32,
    pub cost: u32,
    pub risk: u32,
    pub solver: String,
}

/// Квадратичная форма. `start` — допустимая начальная точка для отжига.
/// Квантовый бэкенд может её не использовать.
#[derive(Debug, Clone)]
pub struct Qubo {
    pub linear: Vec<f64>,
    pub quad: BTreeMap<(usize, usize), f64>,
    pub start: Vec<u8>,
}

impl Qubo {
    pub fn bits(&self) -> usize {
        self.linear.len()
    }

    pub fn energy(&self, x: &[u8]) -> f64 {
        let mut e = 0.0;
        for (i, h) in self.linear.iter().enumerate() {
            if x.get(i).copied().unwrap_or(0) != 0 {
                e += *h;
            }
        }
        for ((i, j), w) in &self.quad {
            if x.get(*i).copied().unwrap_or(0) != 0 && x.get(*j).copied().unwrap_or(0) != 0 {
                e += *w;
            }
        }
        e
    }
}

/// То, чем можно заменить отжиг на железо: на вход QUBO, на выход биты.
pub trait QuboMinimizer: Send {
    fn name(&self) -> &'static str;
    fn minimize(&self, qubo: &Qubo) -> Vec<u8>;
}

pub struct Annealer {
    pub seed: u64,
    pub steps: u32,
}

impl Default for Annealer {
    fn default() -> Self {
        Self {
            seed: 0xC0FFEE,
            steps: 4000,
        }
    }
}

impl QuboMinimizer for Annealer {
    fn name(&self) -> &'static str {
        "qubo-anneal"
    }

    fn minimize(&self, qubo: &Qubo) -> Vec<u8> {
        let n = qubo.bits();
        if n == 0 {
            return Vec::new();
        }
        let mut rng = Lcg(self.seed);
        let mut bits = if qubo.start.len() == n {
            qubo.start.clone()
        } else {
            vec![0; n]
        };
        let mut energy = qubo.energy(&bits);
        let mut best = bits.clone();
        let mut best_e = energy;
        let mut temp = 8.0_f64;
        let steps = self.steps.max(1);
        let cool = (0.01 / temp).powf(1.0 / steps as f64);
        for _ in 0..steps {
            let i = (rng.next() as usize) % n;
            bits[i] ^= 1;
            let next = qubo.energy(&bits);
            let de = next - energy;
            let accept = de <= 0.0 || rng.unit() < (-de / temp).exp();
            if accept {
                energy = next;
                if energy < best_e {
                    best_e = energy;
                    best = bits.clone();
                }
            } else {
                bits[i] ^= 1;
            }
            temp *= cool;
        }
        best
    }
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / ((1u64 << 53) as f64)
    }
}

pub fn problem_from_graph(graph: &TaskGraph, limits: Limits) -> Problem {
    let jobs = graph
        .nodes
        .iter()
        .map(|n| {
            let variants = if n.variants.is_empty() {
                vec![Cost {
                    id: "base".into(),
                    time: n.time_min.max(1),
                    cost: n.cost.max(1),
                    risk: n.risk.max(1),
                }]
            } else {
                n.variants
                    .iter()
                    .map(|v| Cost {
                        id: if v.id.is_empty() {
                            "base".into()
                        } else {
                            v.id.clone()
                        },
                        time: v.time_min.max(1),
                        cost: v.cost.max(1),
                        risk: v.risk.max(1),
                    })
                    .collect()
            };
            Job {
                id: n.id.clone(),
                deps: n.deps.clone(),
                variants,
            }
        })
        .collect();
    Problem { jobs, limits }
}

/// Лимиты, в которые помещается любой один вариант каждого узла, сложенный
/// с остальными максимумами. План остаётся выполнимым, оптимизатор выбирает
/// дешёвый вариант, а не отказывает из-за пустых цифр планировщика.
pub fn loose_limits(problem: &Problem) -> Limits {
    let mut time = 0;
    let mut cost = 0;
    let mut risk = 0;
    for job in &problem.jobs {
        time += job.variants.iter().map(|v| v.time).max().unwrap_or(1);
        cost += job.variants.iter().map(|v| v.cost).max().unwrap_or(1);
        risk += job.variants.iter().map(|v| v.risk).max().unwrap_or(1);
    }
    Limits {
        max_time: time.max(1),
        max_cost: cost.max(1),
        max_risk: risk.max(1),
    }
}

pub fn solve(kind: Kind, problem: &Problem) -> Result<Solution, String> {
    match kind {
        Kind::Qubo => solve_qubo(problem, &Annealer::default()),
        Kind::MiniZinc => solve_external(problem),
    }
}

pub fn solve_qubo(problem: &Problem, backend: &dyn QuboMinimizer) -> Result<Solution, String> {
    if problem.jobs.len() > 10 {
        return Err("QUBO здесь считается до 10 узлов".into());
    }
    let built = build_qubo(problem)?;
    let bits = backend.minimize(&built.qubo);
    decode(problem, &built, &bits, backend.name())
}

fn solve_external(problem: &Problem) -> Result<Solution, String> {
    if problem.jobs.is_empty() {
        return Err("нечего упорядочивать".into());
    }
    if problem.jobs.len() > 12 {
        return Err(
            "внешний решатель зовём до 12 узлов, дальше оставляем порядок планировщика".into(),
        );
    }
    let model = minizinc_model(problem);
    let dir = std::env::temp_dir().join(format!("pcagent-mzn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("plan.mzn");
    std::fs::write(&path, &model).map_err(|e| e.to_string())?;
    let limits = RunLimits {
        timeout: Duration::from_secs(8),
        max_output: 64 * 1024,
    };
    let mz = sandbox::run("minizinc", &[path.display().to_string()], &limits);
    match mz {
        Ok(out) if out.code == Some(0) => {
            if let Ok(sol) = parse_result(problem, &out.stdout, "minizinc") {
                return Ok(sol);
            }
            if out.stdout.contains("UNSATISFIABLE") || out.stderr.contains("UNSATISFIABLE") {
                return Err("MiniZinc: ограничения невыполнимы".into());
            }
            // Модель разобралась, но ответ не тот — пробуем OR-Tools.
        }
        Ok(out) => {
            if out.stdout.contains("UNSATISFIABLE") || out.stderr.contains("UNSATISFIABLE") {
                return Err("MiniZinc: ограничения невыполнимы".into());
            }
        }
        Err(e) => {
            let msg = e.to_string();
            return match solve_ortools(problem) {
                Ok(sol) => Ok(sol),
                Err(e2) => Err(format!("MiniZinc: {msg}; OR-Tools: {e2}")),
            };
        }
    }
    solve_ortools(problem)
}

fn solve_ortools(problem: &Problem) -> Result<Solution, String> {
    let script = ortools_script(problem);
    let dir = std::env::temp_dir().join(format!("pcagent-ort-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("plan.py");
    std::fs::write(&path, &script).map_err(|e| e.to_string())?;
    let limits = RunLimits {
        timeout: Duration::from_secs(8),
        max_output: 64 * 1024,
    };
    let py = if which("python3") {
        "python3"
    } else if which("python") {
        "python"
    } else {
        return Err("нет minizinc и нет python для OR-Tools".into());
    };
    match sandbox::run(py, &[path.display().to_string()], &limits) {
        Ok(out) if out.ok() => parse_result(problem, &out.stdout, "ortools").or_else(|_| {
            Err(format!(
                "OR-Tools не дал план: {}",
                crate::text::truncate(&out.stderr, 180)
            ))
        }),
        Ok(out) => Err(format!(
            "OR-Tools: {}",
            crate::text::truncate(&format!("{} {}", out.stderr, out.stdout), 180)
        )),
        Err(e) => Err(format!("OR-Tools недоступен: {e}")),
    }
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .any(|dir| dir.join(bin).is_file() || dir.join(format!("{bin}.exe")).is_file())
        })
        .unwrap_or(false)
}

/// Точный перебор. Нужен как оракул бенчмарка и как проверка, что QUBO
/// на маленькой задаче попадает в тот же оптимум.
pub fn solve_exact(problem: &Problem) -> Result<Solution, String> {
    let n = problem.jobs.len();
    if n > 8 {
        return Err("точный перебор только до 8 узлов".into());
    }
    let mut order = Vec::new();
    let mut variant = vec![0usize; n];
    let mut best: Option<Solution> = None;
    search(problem, &mut order, &mut variant, &mut best);
    best.ok_or_else(|| "нет порядка, который укладывается в лимиты".into())
}

fn search(
    problem: &Problem,
    order: &mut Vec<usize>,
    variant: &mut [usize],
    best: &mut Option<Solution>,
) {
    let n = problem.jobs.len();
    if order.len() == n {
        if let Some(sol) = eval(problem, order, variant, "exact") {
            let replace = match best {
                None => true,
                Some(cur) => {
                    score(&sol) < score(cur) || (score(&sol) == score(cur) && sol.order < cur.order)
                }
            };
            if replace {
                *best = Some(sol);
            }
        }
        return;
    }
    for i in 0..n {
        if order.contains(&i) {
            continue;
        }
        let ready = problem.jobs[i].deps.iter().all(|d| {
            problem
                .jobs
                .iter()
                .position(|j| &j.id == d)
                .map(|k| order.contains(&k))
                .unwrap_or(false)
        });
        if !ready {
            continue;
        }
        order.push(i);
        for v in 0..problem.jobs[i].variants.len() {
            variant[i] = v;
            search(problem, order, variant, best);
        }
        order.pop();
    }
}

fn eval(problem: &Problem, order: &[usize], variant: &[usize], solver: &str) -> Option<Solution> {
    let mut time = 0u32;
    let mut cost = 0u32;
    let mut risk = 0u32;
    let mut names = Vec::new();
    let mut chosen = BTreeMap::new();
    for &i in order {
        let v = &problem.jobs[i].variants[variant[i]];
        time = time.saturating_add(v.time);
        cost = cost.saturating_add(v.cost);
        risk = risk.saturating_add(v.risk);
        names.push(problem.jobs[i].id.clone());
        chosen.insert(problem.jobs[i].id.clone(), v.id.clone());
    }
    if time > problem.limits.max_time
        || cost > problem.limits.max_cost
        || risk > problem.limits.max_risk
    {
        return None;
    }
    Some(Solution {
        order: names,
        variant: chosen,
        time,
        cost,
        risk,
        solver: solver.into(),
    })
}

fn score(sol: &Solution) -> u32 {
    sol.cost.saturating_mul(1000) + sol.risk.saturating_mul(10) + sol.time
}

struct Built {
    qubo: Qubo,
    pos: Vec<Vec<usize>>,
    var: Vec<Vec<usize>>,
}

fn build_qubo(problem: &Problem) -> Result<Built, String> {
    let n = problem.jobs.len();
    if n == 0 {
        return Err("пустая задача".into());
    }
    let mut pos = vec![vec![0usize; n]; n];
    let mut var = Vec::new();
    let mut bit = 0usize;
    for i in 0..n {
        for p in 0..n {
            pos[i][p] = bit;
            bit += 1;
        }
    }
    for job in &problem.jobs {
        if job.variants.is_empty() {
            return Err(format!("у {} нет вариантов", job.id));
        }
        let mut row = Vec::new();
        for _ in &job.variants {
            row.push(bit);
            bit += 1;
        }
        var.push(row);
    }
    let mut linear = vec![0.0; bit];
    let mut quad: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    let penalty = 10_000.0;
    let add_lin = |linear: &mut [f64], i: usize, w: f64| linear[i] += w;
    let add_q = |quad: &mut BTreeMap<(usize, usize), f64>, a: usize, b: usize, w: f64| {
        if a == b {
            return;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        *quad.entry(key).or_insert(0.0) += w;
    };

    for i in 0..n {
        // (sum_p x_ip - 1)^2 * penalty
        for p in 0..n {
            add_lin(&mut linear, pos[i][p], penalty * -1.0);
            for q in (p + 1)..n {
                add_q(&mut quad, pos[i][p], pos[i][q], penalty * 2.0);
            }
        }
        for p in 0..n {
            add_lin(&mut linear, pos[p][i], penalty * -1.0);
            for q in (p + 1)..n {
                add_q(&mut quad, pos[p][i], pos[q][i], penalty * 2.0);
            }
        }
        let vs = &var[i];
        for (a, &bit_a) in vs.iter().enumerate() {
            add_lin(&mut linear, bit_a, penalty * -1.0);
            for &bit_b in vs.iter().skip(a + 1) {
                add_q(&mut quad, bit_a, bit_b, penalty * 2.0);
            }
        }
    }
    // Позиция 0..n-1. Зависимость j раньше i: запрет pos(i) <= pos(j).
    for (i, job) in problem.jobs.iter().enumerate() {
        for dep in &job.deps {
            let Some(j) = problem.jobs.iter().position(|o| &o.id == dep) else {
                return Err(format!("{} зависит от неизвестного {dep}", job.id));
            };
            for p in 0..n {
                for q in p..n {
                    add_q(&mut quad, pos[i][p], pos[j][q], penalty);
                }
            }
        }
    }
    for (i, job) in problem.jobs.iter().enumerate() {
        for (v, cost) in job.variants.iter().enumerate() {
            let mut w = cost.cost as f64 * 1000.0 + cost.risk as f64 * 10.0 + cost.time as f64;
            if cost.cost > problem.limits.max_cost
                || cost.time > problem.limits.max_time
                || cost.risk > problem.limits.max_risk
            {
                w += 1_000_000.0;
            }
            add_lin(&mut linear, var[i][v], w);
        }
    }

    let start = greedy_bits(problem, &pos, &var, bit);
    Ok(Built {
        qubo: Qubo {
            linear,
            quad,
            start,
        },
        pos,
        var,
    })
}

/// Старт отжига — дорогой допустимый вариант, чтобы поиск шёл вниз, а не
/// начинался уже в оптимуме.
fn greedy_bits(problem: &Problem, pos: &[Vec<usize>], var: &[Vec<usize>], nbits: usize) -> Vec<u8> {
    let mut bits = vec![0u8; nbits];
    let mut placed = Vec::new();
    while placed.len() < problem.jobs.len() {
        let mut progressed = false;
        for (i, job) in problem.jobs.iter().enumerate() {
            if placed.contains(&i) {
                continue;
            }
            let ready = job.deps.iter().all(|d| {
                problem
                    .jobs
                    .iter()
                    .position(|o| &o.id == d)
                    .map(|k| placed.contains(&k))
                    .unwrap_or(false)
            });
            if ready {
                let slot = placed.len();
                bits[pos[i][slot]] = 1;
                let pick = job
                    .variants
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| {
                        v.cost <= problem.limits.max_cost
                            && v.time <= problem.limits.max_time
                            && v.risk <= problem.limits.max_risk
                    })
                    .max_by_key(|(_, v)| v.cost)
                    .map(|(idx, _)| idx)
                    .unwrap_or(0);
                bits[var[i][pick]] = 1;
                placed.push(i);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    bits
}

fn decode(problem: &Problem, built: &Built, bits: &[u8], solver: &str) -> Result<Solution, String> {
    let n = problem.jobs.len();
    let mut order_idx = vec![None; n];
    let mut variant = vec![0usize; n];
    for i in 0..n {
        let mut at = Vec::new();
        for p in 0..n {
            if bits.get(built.pos[i][p]).copied().unwrap_or(0) != 0 {
                at.push(p);
            }
        }
        if at.len() != 1 {
            return Err(format!("{solver}: узел {i} не получил одну позицию"));
        }
        if order_idx[at[0]].is_some() {
            return Err(format!("{solver}: две задачи на одной позиции"));
        }
        order_idx[at[0]] = Some(i);
        let mut vs = Vec::new();
        for (v, bit) in built.var[i].iter().enumerate() {
            if bits.get(*bit).copied().unwrap_or(0) != 0 {
                vs.push(v);
            }
        }
        if vs.len() != 1 {
            return Err(format!("{solver}: узел {i} не выбрал один вариант"));
        }
        variant[i] = vs[0];
    }
    let order: Vec<usize> = order_idx
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| format!("{solver}: дырка в порядке"))?;
    for (place, &i) in order.iter().enumerate() {
        for dep in &problem.jobs[i].deps {
            let j = problem
                .jobs
                .iter()
                .position(|o| &o.id == dep)
                .ok_or_else(|| format!("нет {dep}"))?;
            let dep_at = order.iter().position(|x| *x == j).unwrap_or(n);
            if dep_at >= place {
                return Err(format!("{solver}: зависимость {dep} стоит не раньше"));
            }
        }
    }
    eval(problem, &order, &variant, solver)
        .ok_or_else(|| format!("{solver}: порядок не уложился в лимиты"))
}

pub fn apply_solution(graph: &mut TaskGraph, sol: &Solution) {
    let mut nodes = Vec::new();
    for id in &sol.order {
        if let Some(mut n) = graph.nodes.iter().find(|n| &n.id == id).cloned() {
            if let Some(vid) = sol.variant.get(id) {
                n.chosen = vid.clone();
                if let Some(v) = n.variants.iter().find(|v| &v.id == vid) {
                    n.time_min = v.time_min;
                    n.cost = v.cost;
                    n.risk = v.risk;
                }
            }
            nodes.push(n);
        }
    }
    if nodes.len() == graph.nodes.len() {
        graph.nodes = nodes;
    }
}

fn minizinc_model(problem: &Problem) -> String {
    let n = problem.jobs.len();
    let mut s = String::from("include \"alldifferent.mzn\";\n");
    s.push_str(&format!(
        "int: n = {n};\narray[1..n] of var 1..n: pos;\nconstraint alldifferent(pos);\n"
    ));
    for (i, job) in problem.jobs.iter().enumerate() {
        for dep in &job.deps {
            if let Some(j) = problem.jobs.iter().position(|o| &o.id == dep) {
                s.push_str(&format!("constraint pos[{}] < pos[{}];\n", j + 1, i + 1));
            }
        }
        let nv = job.variants.len();
        let idx = i + 1;
        s.push_str(&format!("var 1..{nv}: v{idx};\n"));
        let times: Vec<_> = job.variants.iter().map(|v| v.time.to_string()).collect();
        let costs: Vec<_> = job.variants.iter().map(|v| v.cost.to_string()).collect();
        let risks: Vec<_> = job.variants.iter().map(|v| v.risk.to_string()).collect();
        s.push_str(&format!(
            "array[1..{nv}] of int: t{idx} = [{times}];\narray[1..{nv}] of int: c{idx} = [{costs}];\narray[1..{nv}] of int: r{idx} = [{risks}];\nvar int: tt{idx} = t{idx}[v{idx}];\nvar int: cc{idx} = c{idx}[v{idx}];\nvar int: rr{idx} = r{idx}[v{idx}];\n",
            times = times.join(", "),
            costs = costs.join(", "),
            risks = risks.join(", "),
        ));
    }
    let tsum = (1..=n)
        .map(|i| format!("tt{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    let csum = (1..=n)
        .map(|i| format!("cc{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    let rsum = (1..=n)
        .map(|i| format!("rr{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    s.push_str(&format!(
        "constraint ({tsum}) <= {};\nconstraint ({csum}) <= {};\nconstraint ({rsum}) <= {};\nsolve minimize ({csum}) * 1000 + ({rsum}) * 10 + ({tsum});\n",
        problem.limits.max_time, problem.limits.max_cost, problem.limits.max_risk
    ));
    let vs = (1..=n)
        .map(|i| format!("v{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    s.push_str(&format!(
        "output [\"RESULT order=\\(pos) variant=[{vs}] cost=\\({csum}) time=\\({tsum}) risk=\\({rsum})\\n\"];\n",
        vs = vs,
        csum = csum,
        tsum = tsum,
        rsum = rsum,
    ));
    s
}

fn ortools_script(problem: &Problem) -> String {
    let n = problem.jobs.len();
    let mut s =
        String::from("from ortools.sat.python import cp_model\nmodel = cp_model.CpModel()\n");
    s.push_str(&format!(
        "pos = [model.new_int_var(0, {m}, 'p%d' % i) for i in range({n})]\nmodel.add_all_different(pos)\n",
        m = n.saturating_sub(1),
        n = n,
    ));
    for (i, job) in problem.jobs.iter().enumerate() {
        for dep in &job.deps {
            if let Some(j) = problem.jobs.iter().position(|o| &o.id == dep) {
                s.push_str(&format!("model.add(pos[{j}] < pos[{i}])\n"));
            }
        }
        let nv = job.variants.len().saturating_sub(1);
        s.push_str(&format!("v{i} = model.new_int_var(0, {nv}, 'v{i}')\n"));
        let times: Vec<_> = job.variants.iter().map(|v| v.time.to_string()).collect();
        let costs: Vec<_> = job.variants.iter().map(|v| v.cost.to_string()).collect();
        let risks: Vec<_> = job.variants.iter().map(|v| v.risk.to_string()).collect();
        s.push_str(&format!(
            "tt{i} = model.new_int_var(0, 100000, 'tt{i}')\ncc{i} = model.new_int_var(0, 100000, 'cc{i}')\nrr{i} = model.new_int_var(0, 100000, 'rr{i}')\nmodel.add_element(v{i}, [{times}], tt{i})\nmodel.add_element(v{i}, [{costs}], cc{i})\nmodel.add_element(v{i}, [{risks}], rr{i})\n",
            times = times.join(", "),
            costs = costs.join(", "),
            risks = risks.join(", "),
        ));
    }
    let tsum = (0..n)
        .map(|i| format!("tt{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    let csum = (0..n)
        .map(|i| format!("cc{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    let rsum = (0..n)
        .map(|i| format!("rr{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    let vs = (0..n)
        .map(|i| format!("solver.value(v{i})+1"))
        .collect::<Vec<_>>()
        .join(", ");
    s.push_str(&format!(
        "model.add(({tsum}) <= {tmax})\nmodel.add(({csum}) <= {cmax})\nmodel.add(({rsum}) <= {rmax})\nmodel.minimize(({csum}) * 1000 + ({rsum}) * 10 + ({tsum}))\nsolver = cp_model.CpSolver()\nst = solver.solve(model)\nif st not in (cp_model.OPTIMAL, cp_model.FEASIBLE):\n    raise SystemExit('UNSAT')\norder = [solver.value(p) + 1 for p in pos]\nvariant = [{vs}]\nprint('RESULT order=' + str(order).replace(' ', '') + ' variant=' + str(variant).replace(' ', '') + ' cost=' + str(solver.value({csum})) + ' time=' + str(solver.value({tsum})) + ' risk=' + str(solver.value({rsum})))\n",
        tmax = problem.limits.max_time,
        cmax = problem.limits.max_cost,
        rmax = problem.limits.max_risk,
        vs = vs,
        tsum = tsum,
        csum = csum,
        rsum = rsum,
    ));
    s
}

fn parse_result(problem: &Problem, text: &str, solver: &str) -> Result<Solution, String> {
    let line = text
        .lines()
        .find(|l| l.contains("RESULT order="))
        .ok_or_else(|| format!("{solver}: нет строки RESULT"))?;
    let order_raw = field(line, "order=").ok_or("нет order")?;
    let variant_raw = field(line, "variant=").ok_or("нет variant")?;
    let positions = ints_in(order_raw);
    let variants = ints_in(variant_raw);
    let n = problem.jobs.len();
    if positions.len() != n || variants.len() != n {
        return Err(format!("{solver}: длина ответа не равна числу узлов"));
    }
    let mut order = vec![0usize; n];
    for (i, p) in positions.iter().enumerate() {
        if *p < 1 || *p > n as i64 {
            return Err(format!("{solver}: позиция вне диапазона"));
        }
        order[(*p as usize) - 1] = i;
    }
    let mut variant = vec![0usize; n];
    for (i, v) in variants.iter().enumerate() {
        if *v < 1 || *v as usize > problem.jobs[i].variants.len() {
            return Err(format!("{solver}: вариант вне диапазона"));
        }
        variant[i] = (*v as usize) - 1;
    }
    eval(problem, &order, &variant, solver)
        .ok_or_else(|| format!("{solver}: ответ нарушает лимиты"))
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.split(key).nth(1)?;
    Some(rest.split_whitespace().next().unwrap_or(rest))
}

fn ints_in(text: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() || (c == '-' && cur.is_empty()) {
            cur.push(c);
        } else if !cur.is_empty() {
            if let Ok(n) = cur.parse() {
                out.push(n);
            }
            cur.clear();
        }
    }
    if let Ok(n) = cur.parse() {
        out.push(n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Problem {
        Problem {
            jobs: vec![
                Job {
                    id: "a".into(),
                    deps: vec![],
                    variants: vec![
                        Cost {
                            id: "expensive".into(),
                            time: 2,
                            cost: 9,
                            risk: 5,
                        },
                        Cost {
                            id: "cheap".into(),
                            time: 1,
                            cost: 1,
                            risk: 1,
                        },
                    ],
                },
                Job {
                    id: "b".into(),
                    deps: vec!["a".into()],
                    variants: vec![Cost {
                        id: "base".into(),
                        time: 1,
                        cost: 1,
                        risk: 1,
                    }],
                },
            ],
            limits: Limits {
                max_time: 10,
                max_cost: 5,
                max_risk: 10,
            },
        }
    }

    #[test]
    fn qubo_matches_exact_optimum_and_respects_dependencies() {
        let problem = sample();
        let exact = solve_exact(&problem).unwrap();
        let qubo = solve_qubo(
            &problem,
            &Annealer {
                seed: 1,
                steps: 4000,
            },
        )
        .unwrap();
        assert_eq!(exact.cost, 2, "{exact:?}");
        assert_eq!(qubo.cost, exact.cost, "QUBO {qubo:?} exact {exact:?}");
        assert_eq!(qubo.variant.get("a").map(String::as_str), Some("cheap"));
        let ia = qubo.order.iter().position(|id| id == "a").unwrap();
        let ib = qubo.order.iter().position(|id| id == "b").unwrap();
        assert!(ia < ib, "{:?}", qubo.order);
        assert_eq!(qubo.solver, "qubo-anneal");
    }

    #[test]
    fn external_solver_is_real_or_reports_that_it_is_missing() {
        let problem = sample();
        match solve(Kind::MiniZinc, &problem) {
            Ok(sol) => {
                assert_eq!(sol.cost, 2, "{sol:?}");
                assert!(
                    sol.solver == "minizinc" || sol.solver == "ortools",
                    "{}",
                    sol.solver
                );
            }
            Err(e) => {
                let low = e.to_lowercase();
                assert!(
                    low.contains("minizinc")
                        || low.contains("or-tools")
                        || low.contains("python")
                        || low.contains("ortools"),
                    "{e}"
                );
            }
        }
    }
}
