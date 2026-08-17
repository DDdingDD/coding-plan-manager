<template>
  <div v-if="bars.length === 0" class="empty">暂无数据</div>
  <div v-else class="token-bars" :style="{ height: `${height}px` }">
    <div class="bar-area">
      <div v-for="b in bars" :key="b.label" class="bar-col">
        <a-tooltip :title="b.tooltip">
          <div class="bar-stack" :style="{ height: `${(b.value / maxValue) * 100}%` }">
            <div
              v-if="b.promptValue > 0"
              class="bar-seg prompt"
              :style="{ height: `${(b.promptValue / b.value) * 100}%` }"
            />
            <div
              v-if="b.cacheValue > 0"
              class="bar-seg cache"
              :style="{ height: `${(b.cacheValue / b.value) * 100}%` }"
            />
            <div
              v-if="b.completionValue > 0"
              class="bar-seg completion"
              :style="{ height: `${(b.completionValue / b.value) * 100}%` }"
            />
          </div>
        </a-tooltip>
      </div>
    </div>
    <div class="label-row">
      <div v-for="b in bars" :key="b.label" class="bar-label" :title="b.label">
        {{ b.label }}
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed } from "vue";

/** 单根柱：label 为横轴文案，value 为主值（决定高度），promptValue/cacheValue/completionValue 拆分为输入/缓存读/输出三段 */
export interface BarItem {
  label: string;
  tooltip: string;
  value: number;
  promptValue: number;
  cacheValue: number;
  completionValue: number;
}

const props = withDefaults(
  defineProps<{ bars: BarItem[]; height?: number }>(),
  { height: 220 },
);

const maxValue = computed(() => Math.max(1, ...props.bars.map((b) => b.value)));
</script>

<script lang="ts">
export default { name: "TokenBars" };
</script>

<style scoped>
.token-bars {
  display: flex;
  flex-direction: column;
}
.empty {
  color: #999;
  padding: 40px 0;
  text-align: center;
}
.bar-area {
  flex: 1;
  display: flex;
  align-items: flex-end;
  gap: 2px;
  min-height: 0;
}
.bar-col {
  flex: 1;
  min-width: 0;
  height: 100%;
  display: flex;
  align-items: flex-end;
  justify-content: center;
}
.bar-stack {
  width: 100%;
  max-width: 36px;
  display: flex;
  flex-direction: column;
  justify-content: flex-end;
  border-radius: 3px 3px 0 0;
  overflow: hidden;
  cursor: default;
}
.bar-seg {
  width: 100%;
}
.bar-seg.prompt {
  background: #1677ff;
}
.bar-seg.cache {
  background: #95de64;
}
.bar-seg.completion {
  background: #69b1ff;
}
.bar-col:hover .bar-seg.prompt {
  background: #0958d9;
}
.bar-col:hover .bar-seg.cache {
  background: #73d13d;
}
.bar-col:hover .bar-seg.completion {
  background: #4096ff;
}
.label-row {
  display: flex;
  gap: 2px;
  border-top: 1px solid #f0f0f0;
  margin-top: 4px;
  padding-top: 4px;
}
.bar-label {
  flex: 1;
  min-width: 0;
  text-align: center;
  font-size: 11px;
  color: #666;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
