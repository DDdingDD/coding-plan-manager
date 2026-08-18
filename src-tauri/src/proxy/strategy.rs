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

/// 无副作用地推导「下一个将被选中的计划」：选择规则与 pick_plan 一致，
/// 但绝不触发清零回绕。供列表/详情查询派生 current_plan_id，
/// 修改轮转规则时两处必须同步。
pub fn peek_plan(conn: &Connection, agg: &Aggregator) -> rusqlite::Result<Option<i64>> {
    let bindings = db::list_bindings(conn, agg.id)?;
    let active: Vec<_> = bindings.iter().filter(|b| b.plan.enabled).collect();
    if active.is_empty() {
        return Ok(None);
    }
    for b in &active {
        if b.used_tokens < agg.token_threshold {
            return Ok(Some(b.plan.id));
        }
    }
    // 全部超额：回绕后用的也是第一个（不清零，用量仍由 pick_plan 在真正转发时重置）
    Ok(Some(active[0].plan.id))
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

    #[test]
    fn peek_matches_pick_without_side_effects() {
        let (conn, agg, p1, p2) = setup();

        // 首个未超额：两者一致
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p1));
        assert_eq!(pick_plan(&conn, &agg).unwrap().unwrap().plan.id, p1);

        // p1 超额 -> 一致切到 p2；跳过禁用计划与 pick 行为相同
        let b = db_mod::list_bindings(&conn, agg.id).unwrap().remove(0);
        db_mod::add_binding_usage(&conn, b.binding_id, 100).unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p2));
        db_mod::update_plan(&conn, p1, "p1", "https://a", "t1", "", false).unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p2));

        // 全部禁用 -> None（与 pick 的 503 语义对应）
        db_mod::update_plan(&conn, p2, "p2", "https://b", "t2", "", false).unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), None);
    }

    #[test]
    fn peek_all_exhausted_wraps_without_reset() {
        let (conn, agg, p1, _p2) = setup();
        for b in db_mod::list_bindings(&conn, agg.id).unwrap() {
            db_mod::add_binding_usage(&conn, b.binding_id, 200).unwrap();
        }

        // 回绕到第一个，但不清零（清零只应发生在真正转发的 pick_plan 里）
        assert_eq!(peek_plan(&conn, &agg).unwrap(), Some(p1));
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens > 0), "peek 不应清零用量");

        // 随后真正 pick 时：清零并选中同一个计划
        let pick = pick_plan(&conn, &agg).unwrap().unwrap();
        assert_eq!(pick.plan.id, p1);
        let bs = db_mod::list_bindings(&conn, agg.id).unwrap();
        assert!(bs.iter().all(|b| b.used_tokens == 0));
    }

    #[test]
    fn peek_none_when_no_bindings() {
        let conn = db_mod::init_db(":memory:").unwrap();
        let agg = db_mod::create_aggregator(&conn, "g", 8300, "cpm", 100).unwrap();
        assert_eq!(peek_plan(&conn, &agg).unwrap(), None);
    }
}
