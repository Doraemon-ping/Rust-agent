<script setup lang="ts">
import { Folder, FolderPlus, LoaderCircle, Trash } from '@lucide/vue'
import { computed, onMounted } from 'vue'
import { useRouter } from 'vue-router'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import { formatRelative, shortenPath } from '@/lib/format'
import type { Project } from '@/lib/projects'
import { RUNTIME_STATE_LABEL, runtimeStateTone } from '@/lib/runtime/types'
import { useProjectsStore } from '@/stores/projects'
import { useRuntimeStore } from '@/stores/runtime'

const router = useRouter()
const projects = useProjectsStore()
const runtime = useRuntimeStore()

const stateTone = computed(() => runtimeStateTone(runtime.state))

onMounted(() => {
  void projects.refresh()
})

/** Choosing a project is what gives the chat its subject. */
async function open(project: Project): Promise<void> {
  await projects.select(project.id)
  await router.push('/')
}
</script>

<template>
  <aside
    class="flex w-72 shrink-0 flex-col border-r border-line bg-surface/30"
    aria-label="项目"
  >
    <header class="flex items-center justify-between gap-2 py-4 pr-3 pl-4">
      <h2 class="text-sm font-semibold tracking-tight">项目</h2>
      <Button
        size="sm"
        variant="outline"
        :disabled="!projects.shell || projects.busy"
        :title="projects.shell ? '选择一个文件夹作为项目' : '桌面宿主未运行'"
        @click="projects.add()"
      >
        <FolderPlus class="size-3.5" />
        添加项目
      </Button>
    </header>

    <div class="min-h-0 flex-1 overflow-y-auto px-3 pb-3 scroll-slim">
      <Alert v-if="projects.error" tone="danger" title="项目操作失败" class="mb-3">
        <p class="font-mono text-xs">{{ projects.error.code }}</p>
        <p class="break-words">{{ projects.error.message }}</p>
      </Alert>

      <Alert v-if="projects.warnings.length" tone="warn" title="项目列表有问题" class="mb-3">
        <ul class="list-disc space-y-0.5 pl-4">
          <li
            v-for="warning in projects.warnings"
            :key="warning"
            class="break-words"
          >
            {{ warning }}
          </li>
        </ul>
      </Alert>

      <ul v-if="projects.items.length" class="space-y-1">
        <li
          v-for="project in projects.items"
          :key="project.id"
          class="group flex items-stretch rounded-lg transition-colors"
          :class="project.id === projects.activeId ? 'bg-surface-raised' : 'hover:bg-surface-raised'"
        >
          <button
            type="button"
            class="flex min-w-0 flex-1 items-start gap-2.5 rounded-lg px-2.5 py-2 text-left"
            :title="`${project.path}\n添加于 ${formatRelative(project.addedAt)}`"
            @click="open(project)"
          >
            <Folder
              class="mt-0.5 size-4 shrink-0"
              :class="project.exists ? 'text-ink-faint' : 'text-warn'"
            />
            <span class="min-w-0 flex-1">
              <span class="flex items-center gap-2">
                <span
                  class="truncate text-sm"
                  :class="project.id === projects.activeId ? 'text-ink' : 'text-ink-muted'"
                >
                  {{ project.name }}
                </span>
                <Badge
                  v-if="project.id === projects.activeId"
                  variant="accent"
                  class="shrink-0 px-1.5 py-0 text-[10px]"
                >
                  当前
                </Badge>
                <Badge
                  v-if="!project.exists"
                  variant="warn"
                  class="shrink-0 px-1.5 py-0 text-[10px]"
                >
                  目录缺失
                </Badge>
              </span>
              <span class="mt-0.5 block truncate font-mono text-[11px] text-ink-faint">
                {{ shortenPath(project.path, 32) }}
              </span>
            </span>
          </button>

          <button
            type="button"
            class="my-1 mr-1 shrink-0 self-start rounded p-1.5 text-ink-faint opacity-0 transition hover:text-danger focus-visible:opacity-100 group-hover:opacity-100"
            :title="`移除「${project.name}」（不会删除文件夹）`"
            :aria-label="`移除「${project.name}」`"
            @click="projects.remove(project.id)"
          >
            <Trash class="size-3.5" />
          </button>
        </li>
      </ul>

      <p
        v-else-if="!projects.loaded"
        class="flex items-center justify-center gap-2 px-2 py-6 text-xs text-ink-faint"
      >
        <LoaderCircle class="size-3.5 animate-spin" />
        正在读取项目列表…
      </p>

      <p v-else class="px-3 py-6 text-center text-xs leading-relaxed text-ink-faint">
        还没有项目。点「添加项目」选一个文件夹，runtime 会在里面运行。
      </p>
    </div>

    <footer class="space-y-2 border-t border-line px-4 py-3">
      <div class="flex items-center gap-2 text-xs">
        <span class="text-ink-faint">Runtime</span>
        <Badge :variant="stateTone" class="ml-auto">
          {{ RUNTIME_STATE_LABEL[runtime.state] }}
        </Badge>
      </div>
      <p class="text-[11px] leading-relaxed text-ink-faint">
        选中的项目就是 runtime 的工作目录：它会在这个文件夹里读写记忆、任务与日志。没选项目时，用的是应用数据目录。
      </p>
    </footer>
  </aside>
</template>
