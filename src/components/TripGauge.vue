<template>
  <div class="trip-gauge-wrap">
    <div
      class="trip-gauge"
      :class="{ paused: trip.paused }"
      :title="gaugeTitle"
      @click="onClick"
      @dblclick="onDblClick"
    >
      <div class="gauge-label">{{ trip.paused ? "已暂停" : "小计" }}</div>
      <div class="gauge-value">{{ displayTokens }}</div>
      <div class="gauge-unit">tokens</div>
      <div class="gauge-requests">{{ displayRequests }} 次请求</div>
    </div>
    <div class="gauge-start">{{ tripStartLabel }}</div>
  </div>
</template>

<script setup lang="ts">
import { computed, onUnmounted, ref, watch, toRef, type Ref } from "vue";
import type { TripStats } from "../types";

const props = defineProps<{ trip: TripStats }>();
const emit = defineEmits<{ (e: "reset"): void; (e: "toggle-pause"): void }>();

// 单击=暂停/继续，双击=重置：单击延迟 250ms 等待是否构成双击
let clickTimer: ReturnType<typeof setTimeout> | null = null;
function onClick() {
  if (clickTimer) return; // 双击的第二击，由 dblclick 处理
  clickTimer = setTimeout(() => {
    clickTimer = null;
    emit("toggle-pause");
  }, 250);
}
function onDblClick() {
  if (clickTimer) {
    clearTimeout(clickTimer);
    clickTimer = null;
  }
  emit("reset");
}
onUnmounted(() => {
  if (clickTimer) clearTimeout(clickTimer);
});

/** 数字滚动过渡：source 变化时从当前显示值 rAF 插值到新值（easeOutCubic），连击不跳变 */
function useTweenedNumber(source: Ref<number>, duration = 600) {
  const display = ref(source.value);
  let raf = 0;
  watch(source, (to) => {
    cancelAnimationFrame(raf);
    const begin = display.value;
    if (begin === to) return;
    const start = performance.now();
    const step = (t: number) => {
      const p = Math.min(1, (t - start) / duration);
      const e = 1 - Math.pow(1 - p, 3);
      display.value = Math.round(begin + (to - begin) * e);
      if (p < 1) raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
  });
  onUnmounted(() => cancelAnimationFrame(raf));
  return display;
}

/** 圆盘内紧凑格式：圆盘宽约 130px，完整千分位在百万级会溢出 */
function formatCompact(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(2)}M`;
  return n.toLocaleString();
}

const tweenedTokens = useTweenedNumber(toRef(() => props.trip.stats.total_tokens));
const tweenedRequests = useTweenedNumber(toRef(() => props.trip.stats.requests));

const displayTokens = computed(() => formatCompact(tweenedTokens.value));
const displayRequests = computed(() => tweenedRequests.value.toLocaleString());

const tripStartLabel = computed(() =>
  props.trip.started_at ? `自 ${props.trip.started_at.slice(5, 16)}` : "未重置",
);

const gaugeTitle = computed(() => {
  const exact = `${props.trip.stats.total_tokens.toLocaleString()} tokens · ${props.trip.stats.requests.toLocaleString()} 次请求`;
  const since = props.trip.started_at
    ? `自 ${props.trip.started_at} 起`
    : "尚未重置，计入全部历史";
  const state = props.trip.paused ? "已暂停（暂停期间的用量不计入）" : "计数中";
  return `${exact}\n${since} · ${state}\n单击暂停/继续 · 双击重置`;
});
</script>

<style scoped>
.trip-gauge-wrap {
  display: flex;
  flex-direction: column;
  align-items: center;
}
.trip-gauge {
  width: 130px;
  height: 130px;
  border-radius: 50%;
  border: 2px solid #d9d9d9;
  background: radial-gradient(circle at 50% 35%, #ffffff 60%, #f5f5f5 100%);
  box-shadow: inset 0 0 12px rgba(0, 0, 0, 0.04);
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  user-select: none;
  transition:
    border-color 0.15s,
    box-shadow 0.15s,
    transform 0.15s;
}
.trip-gauge:hover {
  border-color: #1677ff;
  box-shadow:
    inset 0 0 12px rgba(0, 0, 0, 0.04),
    0 0 0 3px rgba(22, 119, 255, 0.12);
}
.trip-gauge:active {
  transform: scale(0.96);
}
/* 暂停态：琥珀边框 + 灰数字，与计数中（蓝）区分 */
.trip-gauge.paused {
  border-color: #faad14;
}
.trip-gauge.paused:hover {
  border-color: #faad14;
  box-shadow:
    inset 0 0 12px rgba(0, 0, 0, 0.04),
    0 0 0 3px rgba(250, 173, 20, 0.15);
}
.trip-gauge.paused .gauge-value {
  color: #8c8c8c;
}
.trip-gauge.paused .gauge-label {
  color: #faad14;
}
.gauge-label {
  font-size: 11px;
  color: #8c8c8c;
  letter-spacing: 2px;
  transition: color 0.15s;
}
.gauge-value {
  font-size: 22px;
  font-weight: 600;
  color: #1f1f1f;
  line-height: 1.3;
  font-variant-numeric: tabular-nums;
  max-width: 110px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  transition: color 0.15s;
}
.gauge-unit {
  font-size: 11px;
  color: #8c8c8c;
}
.gauge-requests {
  margin-top: 4px;
  font-size: 12px;
  color: #595959;
  font-variant-numeric: tabular-nums;
}
.gauge-start {
  margin-top: 6px;
  font-size: 12px;
  color: #8c8c8c;
}
</style>
