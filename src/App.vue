<template>
  <a-layout style="height: 100vh">
    <a-layout-sider theme="light" :width="200" style="border-right: 1px solid #f0f0f0">
      <div class="logo">Coding Plan Manager</div>
      <a-menu v-model:selectedKeys="current" mode="inline" :items="menuItems" />
      <div class="global-stats">
        <div class="tray-setting">
          <a-switch size="small" v-model:checked="hideOnClose" />
          <span>关闭时隐藏到托盘</span>
        </div>
        <a-statistic title="累计 Token" :value="stats.total_tokens" style="margin-bottom: 4px" />
        <a-statistic title="累计请求" :value="stats.requests" />
      </div>
    </a-layout-sider>
    <a-layout-content style="padding: 16px; overflow: auto">
      <PlansPage v-if="current[0] === 'plans'" />
      <AggregatorsPage v-else-if="current[0] === 'aggregators'" />
      <MessagesPage v-else-if="current[0] === 'messages'" />
      <StatsPage v-else />
    </a-layout-content>
  </a-layout>
</template>

<script setup lang="ts">
import { h, onMounted, onUnmounted, ref, watch, type Component } from "vue";
import {
  ApiOutlined,
  BarChartOutlined,
  ClusterOutlined,
  MessageOutlined,
} from "@ant-design/icons-vue";
import { Modal } from "ant-design-vue";
import { getCurrentWindow } from "@tauri-apps/api/window";
import PlansPage from "./pages/PlansPage.vue";
import AggregatorsPage from "./pages/AggregatorsPage.vue";
import MessagesPage from "./pages/MessagesPage.vue";
import StatsPage from "./pages/StatsPage.vue";
import { globalStats, onNewMessage } from "./api";
import type { UsageStats } from "./types";

const current = ref<string[]>(["aggregators"]);

const menuItems = [
  {
    key: "aggregators",
    icon: () => h(ClusterOutlined),
    label: "聚合器",
  },
  {
    key: "plans",
    icon: () => h(ApiOutlined),
    label: "Coding Plan",
  },
  {
    key: "messages",
    icon: () => h(MessageOutlined),
    label: "消息记录",
  },
  {
    key: "stats",
    icon: () => h(BarChartOutlined),
    label: "统计",
  },
];

const stats = ref<UsageStats>({
  total_tokens: 0,
  prompt_tokens: 0,
  completion_tokens: 0,
  cache_read_tokens: 0,
  cache_creation_tokens: 0,
  requests: 0,
});

const refreshStats = () => globalStats().then((s) => (stats.value = s));

let unlisten: (() => void) | null = null;
let unlistenClose: (() => void) | null = null;

// 关闭窗口行为偏好：true=隐藏到托盘，false=弹退出确认框；localStorage 持久化，默认开启
const HIDE_ON_CLOSE_KEY = "cpm.hideOnClose";
const hideOnClose = ref(localStorage.getItem(HIDE_ON_CLOSE_KEY) !== "0");
watch(hideOnClose, (v) => localStorage.setItem(HIDE_ON_CLOSE_KEY, v ? "1" : "0"));

onMounted(async () => {
  refreshStats();
  unlisten = await onNewMessage(() => refreshStats());
  // 拦截关闭：开启偏好时直接隐藏到托盘；否则弹确认框，确认后用 destroy() 绕过 close-requested 真正退出
  unlistenClose = await getCurrentWindow().onCloseRequested((event) => {
    event.preventDefault();
    if (hideOnClose.value) {
      getCurrentWindow().hide();
      return;
    }
    Modal.confirm({
      title: "确定要退出程序吗？",
      content: "退出后将停止所有运行中的聚合器服务。",
      okText: "退出",
      cancelText: "取消",
      onOk: () => getCurrentWindow().destroy(),
    });
  });
});
onUnmounted(() => {
  unlisten?.();
  unlistenClose?.();
});
</script>

<style>
body {
  margin: 0;
}
.logo {
  height: 56px;
  display: flex;
  align-items: center;
  justify-content: center;
  font-weight: 600;
  font-size: 15px;
  color: #1f1f1f;
  border-bottom: 1px solid #f0f0f0;
}
.global-stats {
  padding: 16px;
  border-top: 1px solid #f0f0f0;
  position: absolute;
  bottom: 0;
  width: 200px;
  background: #fff;
}
.tray-setting {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 12px;
  font-size: 12px;
  color: #595959;
}
.ant-statistic-title {
  font-size: 12px;
  margin-bottom: 0;
}
</style>
