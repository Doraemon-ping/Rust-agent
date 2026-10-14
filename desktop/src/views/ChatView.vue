<script setup lang="ts">
import { FolderOpen, LoaderCircle, MessageSquare, SendHorizontal } from '@lucide/vue'
import { computed } from 'vue'

import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import { shortenPath } from '@/lib/format'
import { RUNTIME_STATE_LABEL, runtimeStateTone } from '@/lib/runtime/types'
import { useProjectsStore } from '@/stores/projects'
import { useRuntimeStore } from '@/stores/runtime'

const projects = useProjectsStore()
const runtime = useRuntimeStore()

const active = computed(() => projects.active)
const stateTone = computed(() => runtimeStateTone(runtime.state))

/**
 * What the runtime says it can do, straight off `runtime.status`.
 *
 * The chat is not built, and this list is the honest reason: the bridge answers
 * these three methods and nothing that takes a prompt.
 */
const methods = computed(() => runtime.snapshot?.methods ?? [])
</script>

<template>
  <div class="flex h-full min-h-0 flex-col">
    <header class="flex items-start justify-between gap-4 border-b border-line px-6 py-4">
      <div class="min-w-0">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">对话</p>
        <h1 class="mt-1 truncate text-lg font-semibold tracking-tight">
          {{ active ? active.name : '未选择项目' }}
        </h1>
        <p
          class="mt-0.5 truncate font-mono text-xs text-ink-faint"
          :title="active?.path ?? ''"
        >
          {{ active ? shortenPath(active.path, 72) : '在左侧「项目」栏添加或选择一个文件夹' }}
        </p>
      </div>
      <Badge :variant="stateTone">{{ RUNTIME_STATE_LABEL[runtime.state] }}</Badge>
    </header>

    <div class="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 px-6 py-10">
      <template v-if="!projects.loaded && !projects.error">
        <LoaderCircle class="size-6 animate-spin text-ink-faint" />
        <p class="text-xs text-ink-faint">正在从宿主读取项目列表…</p>
      </template>

      <template v-else-if="!active">
        <FolderOpen class="size-8 text-ink-faint" />
        <h2 class="text-base font-semibold tracking-tight">还没有可对话的项目</h2>
        <p
          v-if="projects.error"
          class="max-w-md text-center text-sm break-words text-danger"
        >
          项目列表没有读到（{{ projects.error.code }}）：{{ projects.error.message }}
        </p>
        <p v-else class="max-w-md text-center text-sm text-ink-muted">
          在左侧「项目」栏点「添加项目」选一个文件夹。项目就是 runtime
          的工作目录，选好之后这里会显示它。
        </p>
      </template>

      <template v-else>
        <MessageSquare class="size-8 text-ink-faint" />
        <h2 class="text-base font-semibold tracking-tight">对话尚未接入</h2>
        <p class="max-w-lg text-center text-sm text-ink-muted">
          桌面外壳已经能启动 runtime，但协议 v1 只回答运行时自己的状态，没有接收提示词的方法。
        </p>
        <ul v-if="methods.length" class="flex flex-wrap justify-center gap-2">
          <li
            v-for="method in methods"
            :key="method"
            class="rounded-md border border-line px-2 py-1 font-mono text-[11px] text-ink-faint"
          >
            {{ method }}
          </li>
        </ul>
        <p class="max-w-lg text-center text-xs leading-relaxed text-ink-faint">
          <template v-if="methods.length">上面是 runtime 自己上报的方法。</template>
          <template v-else>runtime 还没有上报方法列表。</template>
          下一步是在 bridge 里加一个接收提示词、流式回答案的方法；在那之前，这一页不会显示任何伪造的回复。
        </p>
      </template>
    </div>

    <footer class="border-t border-line px-6 py-4">
      <div class="flex items-end gap-3">
        <textarea
          class="h-20 min-h-20 w-full resize-none rounded-lg border border-line bg-surface px-3 py-2 text-sm text-ink placeholder:text-ink-faint focus:outline-none disabled:opacity-60"
          placeholder="对话接口尚未接入"
          disabled
        />
        <Button disabled>
          <SendHorizontal class="size-4" />
          发送
        </Button>
      </div>
      <p class="mt-2 text-[11px] text-ink-faint">
        这个输入框是禁用的，不是待发送内容：协议 v1 没有提示词方法，宿主也就不会伪造一条回复。
      </p>
    </footer>
  </div>
</template>

