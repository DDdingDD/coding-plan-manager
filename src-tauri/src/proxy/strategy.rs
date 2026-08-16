use crate::db::{self, Aggregator, CodingPlan};
use rusqlite::Connection;

/// 一次转发选中的计划
#[derive(Debug, Clone)]
pub struct StrategyPick {
    pub binding_id: i64,
    pub plan: CodingPlan,
}

/// 阈值轮转策略：
/// 1. 按绑定顺序取第一个「启用中 且 已用 token < 阈值」的计划
/// 2. 所有计划都达到阈值 -> 该聚合器全部绑定的用量清零，回到第一个重新计数
/// 3. 没有任何启用的计划 -> None（由调用方返回 503）
pub fn pick_plan(conn: &Connection, agg: &Aggregator) -> rusqlite::Result<Option<StrategyPick>> {
    let bindings = db::list_bindings(conn, agg.id)?;
    let active: Vec<_> = bindings.iter().filter(|b| b.plan.enabled).collect();
    if active.is_empty() {
        return Ok(None);
    }

    for b in &active {
        if b.used_tokens < agg.token_threshold {
            return Ok(Some(StrategyPick {
                binding_id: b.binding_id,
                plan: b.plan.clone(),
            }));
        }
    }

    // 全部超额：清零并回绕到第一个
    db::reset_binding_usage(conn, agg.id)?;
    let first = active[0];
    Ok(Some(StrategyPick {
        binding_id: first.binding_id,
        plan: first.plan.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db as db_mod;

    fn setup() -> (Connection, Aggregator, i64, i64) {
        let conn = db_mod::init_db(":memory:").unwrap();
        let p1 = db_mod::create_plan(&conn, "p1", "https://a", "t1", "").unwrap();
        let p2 = db_mod::create_plan(&conn, "p2", "https://b", "t2", "").unwrap();
        let agg = db_mod::create_aggregator(&conn, "g", 8300, "cpm", 100).unwrap();
        db_mod::set_aggregator_plans(&conn, agg.id, &[p1.id, p2.id]).unwrap();
        (conn, agg, p1.id, p2.id)
    }

    #[test]
    fn picks_first_then_next() {
        let (conn, agg, p1, p2) = setup();
        let pick = pick_plan(&conn, &agg).unwrap().unwrap();
        assert_eq!(pick.plan.id, p1);

        // p1 达到阈值 -> 切到 p2
        let b = db_mod::list_bindings(&conn, agg.id).unwrap().remove(0);
        db_mod::add_binding_usage(&conn, b.binding_id, 100).unwrap();
        let pick = pick_plan(&conn, &agg).unwrap().unwrap();
        assert_eq!(pick.plan.id, p2);

        // 唯一启用的 p1 已超额：清零回绕继续用 p1（符合"全部超额->清零重来"语义）
        db_mod::update_plan(&conn, p2, "p2", "https://b", "t2", "", false).unwrap();
        let pick = pick_plan(&conn, &agg).unwrap().unwrap();
        assert_eq!(pick.plan.id, p1);

        // 全部禁用 -> 无可用计划
        db_mod::update_plan(&conn, p1, "p1", "https://a", "t1", "", false).unwrap();
        assert!(pick_plan(&conn, &agg).unwrap().is_none());
    }

    #[test]
    fn all_exhausted_resets_and_wraps() {
        let (conn, agg, p1, p2) = setup();
        for b in db_mod::list_bindings(&conn, agg.id).unwrap() {
            db_mod::add_binding_usage(&conn, b.binding_id, 200).unwrap();
        }
        let pick = pick_plan(&conn, &agg).unwrap().unwrap();
        assert_eq!(pick.plan.id, p1); // 回绕到第一个

        // 所有绑定已被清零
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens == 0));
        let _ = p2;
    }

    #[test]
    fn skips_disabled() {
        let (conn, agg, p1, p2) = setup();
        db_mod::update_plan(&conn, p1, "p1", "https://a", "t1", "", false).unwrap();
        let pick = pick_plan(&conn, &agg).unwrap().unwrap();
        assert_eq!(pick.plan.id, p2);
    }
}
