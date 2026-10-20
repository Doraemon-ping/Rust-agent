<script setup lang="ts">
import {
  CircleAlert,
  CircleCheck,
  LoaderCircle,
  Plug,
  Puzzle,
  RefreshCw,
  ShieldCheck,
  ShieldQuestion,
} from '@lucide/vue'
import { computed, onMounted } from 'vue'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import Card from '@/components/ui/Card.vue'
import { usePluginsStore } from '@/stores/plugins'
import { useRuntimeStore } from '@/stores/runtime'

const plugins = usePluginsStore()
const runtime = useRuntimeStore()

onMounted(() => {
  void plugins.refresh()
})

/** The one honest reason the page cannot show the plugin list. */
const issue = computed(() => {
  if (!runtime.shell) return '当前页面不在桌面宿主中：没有 Runtime 通道，读不到插件。'
  if (runtime.state === 'error') {
    return runtime.lastError
      ? `${runtime.lastError.code}: ${runtime.lastError.message}`
      : 'Runtime 处于错误状态，详情见设置。'
  }
  if (!runtime.ready) return 'Runtime 还没有运行。'
  if (!plugins.supported) {
    return '这个 Runtime 没有上报 plugins.list 方法，因此看不到它的插件。'
  }
  return null
})

/** The one word that fits what the host policy does with a tool. */
function policyLabel(policy: string): string {
  return policy === 'allow' ? '放行' : '需确认'
}

/** The workspace the runtime runs in, which is where a plugin file is read from. */
const workspace = computed(
  () => runtime.hostInfo?.workspace ?? runtime.snapshot?.workspace ?? '工作目录',
)

/** One file the page can show in full: the shape a plugin declaration takes. */
const PLUGIN_EXAMPLE = `{
  "servers": [
    {
      "name": "notes",
      "command": "node",
      "args": ["tools/notes-mcp.mjs"],
      "policy": "allow"
    }
  ]
}`
</script>

<template>
  <div class="h-full min-h-0 overflow-y-auto p-8 scroll-slim">
    <header class="mb-5 flex items-start justify-between gap-4">
      <div class="min-w-0">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">Workbench</p>
        <h1 class="mt-1 text-xl font-semibold tracking-tight">插件</h1>
        <p class="mt-1 max-w-2xl text-sm text-ink-muted">
          插件是 MCP 服务器：连上之后，它提供的工具以
          <span class="font-mono">mcp__&lt;服务器&gt;__&lt;工具&gt;</span>
          进入模型的工具池。这一页显示的是 Runtime 自己的注册表，模型用
          <span class="font-mono">connect_mcp</span> 连的是同一张。
        </p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <Badge v-if="plugins.loaded">
          {{ plugins.connected.length }} / {{ plugins.servers.length }} 已连接
        </Badge>
        <Button
          variant="outline"
          size="sm"
          :disabled="!plugins.ready || plugins.loading"
          @click="plugins.refresh()"
        >
          <RefreshCw class="size-3.5" :class="plugins.loading ? 'animate-spin' : ''" />
          刷新
        </Button>
      </div>
    </header>

    <Alert v-if="issue" tone="info" class="mb-5" title="现在还看不到插件表">
      <p>{{ issue }}</p>
      <p v-if="!runtime.ready" class="text-xs">
        插件的连接状态活在 Runtime 进程里；它不在时这一页读不到任何东西。
      </p>
    </Alert>

    <Alert v-else-if="plugins.error" tone="danger" class="mb-5" :title="plugins.error.code">
      <p>{{ plugins.error.message }}</p>
    </Alert>

    <Card>
      <template #header>
        <div class="min-w-0">
          <h2 class="text-base font-semibold tracking-tight">可连接的插件</h2>
          <p class="text-xs text-ink-faint">
            {{ plugins.servers.length }} 个可用
            <span v-if="plugins.remaining"> · {{ plugins.remaining }} 个还没连接</span>
          </p>
        </div>
      </template>

      <ul v-if="plugins.servers.length" class="divide-y divide-line/60">
        <li
          v-for="server in plugins.servers"
          :key="server.name"
          class="flex flex-wrap items-start justify-between gap-3 py-4 first:pt-0 last:pb-0"
        >
          <div class="min-w-0 flex-1 space-y-2">
            <div class="flex flex-wrap items-center gap-2">
              <span class="font-mono text-sm text-ink">{{ server.name }}</span>
              <Badge
                v-if="server.connected"
                variant="ok"
                class="px-1.5 py-0 text-[10px]"
              >
                <CircleCheck class="size-3" />
                已连接
              </Badge>
              <Badge v-else class="px-1.5 py-0 text-[10px]">未连接</Badge>
              <Badge
                v-if="server.source === 'workspace'"
                class="px-1.5 py-0 text-[10px]"
                title="由工作目录里的 plugins.json 声明"
              >
                工作区声明
              </Badge>
            </div>

            <p
              v-if="server.command"
              class="truncate font-mono text-[11px] text-ink-faint"
              :title="server.command"
            >
              {{ server.command }}
            </p>

            <ul v-if="server.tools.length" class="space-y-1.5">
              <li
                v-for="tool in server.tools"
                :key="tool.name"
                class="flex flex-wrap items-baseline gap-x-2 gap-y-1"
              >
                <span class="font-mono text-xs text-ink-muted">{{ tool.name }}</span>
                <Badge
                  :variant="tool.policy === 'allow' ? 'ok' : 'warn'"
                  class="px-1.5 py-0 text-[10px]"
                >
                  <ShieldCheck v-if="tool.policy === 'allow'" class="size-3" />
                  <ShieldQuestion v-else class="size-3" />
                  {{ policyLabel(tool.policy) }}
                </Badge>
                <span class="min-w-0 text-xs text-ink-faint">{{ tool.description }}</span>
              </li>
            </ul>
            <p v-else class="text-xs text-ink-faint">
              未连接，还没有发现它的工具——目录是连接时向服务器要来的。
            </p>
          </div>

          <div class="flex shrink-0 items-center gap-1.5">
            <Button
              v-if="!server.connected"
              size="sm"
              :disabled="!plugins.canChange || plugins.connecting === server.name"
              @click="plugins.connect(server.name)"
            >
              <LoaderCircle
                v-if="plugins.connecting === server.name"
                class="size-3.5 animate-spin"
              />
              <Plug v-else class="size-3.5" />
              连接
            </Button>
            <span v-else class="flex items-center gap-1.5 text-xs text-ink-faint">
              <Puzzle class="size-3.5" />
              工具已进入工具池
            </span>
          </div>
        </li>
      </ul>

      <div v-else class="py-2 text-sm text-ink-faint">
        <p v-if="plugins.loaded">这个 Runtime 没有可连接的插件。</p>
        <p v-else>还没有读到插件表。</p>
      </div>

      <template #footer>
        <span v-if="plugins.loading" class="flex items-center gap-2">
          <LoaderCircle class="size-3.5 animate-spin" />
          读取插件表…
        </span>
        <span v-else>
          工具是否放行由宿主裁定，不问服务器自己的说法：<span class="font-mono">allow</span>
          直接运行，<span class="font-mono">confirm</span>
          需要确认，非交互回合里会被拒绝而不是挂起。
        </span>
      </template>
    </Card>

    <p class="mt-5 flex items-start gap-2 text-xs text-ink-faint">
      <CircleAlert class="mt-px size-3.5 shrink-0" />
      <span>
        连接在 Runtime 本次运行期间有效：重启 Runtime 回到未连接，需要重连。列表里没有「停用」，因为
        Runtime 的注册表只能加不能减。
      </span>
    </p>

    <Card class="mt-5">
      <template #header>
        <div class="min-w-0">
          <h2 class="text-base font-semibold tracking-tight">扩展插件</h2>
          <p class="text-xs text-ink-faint">
            在工作目录放一个 <span class="font-mono">plugins.json</span>，就能把自己的 MCP
            服务器加进注册表。
          </p>
        </div>
      </template>

      <div class="space-y-4 text-sm text-ink-muted">
        <p>
          插件表由工作目录里的 <span class="font-mono">plugins.json</span>
          声明，Runtime 启动时读一次。这次运行的工作目录是
          <span class="font-mono break-all">{{ workspace }}</span>。
        </p>

        <pre
          class="overflow-x-auto rounded-panel border border-line bg-canvas px-4 py-3 font-mono text-xs text-ink-muted scroll-slim"
        >{{ PLUGIN_EXAMPLE }}</pre>

        <ul class="list-disc space-y-1.5 pl-5">
          <li>
            <span class="font-mono">servers[]</span> 里每一项声明一个用标准输入输出通信的 MCP
            服务器：<span class="font-mono">name</span> 是它的名字，<span class="font-mono">command</span>
            和 <span class="font-mono">args</span> 是启动它的命令行，<span class="font-mono">env</span>
            可选地补充环境变量。
          </li>
          <li>
            <span class="font-mono">policy</span>（<span class="font-mono">allow</span> 或
            <span class="font-mono">confirm</span>）决定它的工具默认是放行还是需要确认；不写就跟随宿主策略。
          </li>
          <li>
            声明过的服务器会出现在上面的列表里，带「工作区声明」标记，模型的
            <span class="font-mono">connect_mcp</span> 也认它。
          </li>
          <li>
            名字和内置插件重名、缺 <span class="font-mono">name</span> 或
            <span class="font-mono">command</span>、或文件不是合法 JSON，Runtime
            会报错并把整个文件跳过，不会加进来。
          </li>
          <li>
            <span class="font-mono">plugins.json</span> 只在启动时读，新增服务器后要重启 Runtime 才生效。
          </li>
        </ul>
      </div>
    </Card>
  </div>
</template>
