import { t } from '../../i18n'
import {
  IconAgentSpawn,
  IconAgentFork,
  IconToolMessage,
  IconToolInterrupt,
  IconAgentList,
  IconToolWait,
  IconJobStart,
  IconJobList,
  IconJobOutput,
  IconToolSkill,
  IconToolAsk,
  IconTerminal,
  IconRead,
  IconWrite,
  IconGrep,
  IconGlob,
  IconFolder,
  IconEdit,
  IconTodo,
  IconTool,
} from '../icons'

/** 工具行头部的标题与图标。 */
export function toolMeta(name: string): { title: string; icon: JSX.Element } {
  switch (name) {
    case 'spawn_agent': return { title: t('runtimeAgents'), icon: <IconAgentSpawn size={14} /> }
    case 'fork_agent': return { title: t('runtimeAgents'), icon: <IconAgentFork size={14} /> }
    case 'send_message': return { title: t('runtimeMessage'), icon: <IconToolMessage size={14} /> }
    case 'interrupt_agent': return { title: t('runtimeInterrupt'), icon: <IconToolInterrupt size={14} /> }
    case 'job_kill': return { title: t('runtimeInterrupt'), icon: <IconToolInterrupt size={14} /> }
    case 'list_agents': return { title: t('runtimeAgents'), icon: <IconAgentList size={14} /> }
    case 'wait_agent': return { title: t('runtimeAgents'), icon: <IconToolWait size={14} /> }
    case 'job_start': return { title: t('runtimeJobs'), icon: <IconJobStart size={14} /> }
    case 'job_list': return { title: t('runtimeJobs'), icon: <IconJobList size={14} /> }
    case 'job_output': return { title: t('runtimeOutput'), icon: <IconJobOutput size={14} /> }
    case 'skill': return { title: t('runtimeSkills'), icon: <IconToolSkill size={14} /> }
    case 'ask': return { title: t('askTitle'), icon: <IconToolAsk size={14} /> }

    case 'bash':
      return { title: 'Bash', icon: <IconTerminal size={14} /> }
    case 'read_file':
      return { title: 'Read', icon: <IconRead size={14} /> }
    case 'write_file':
      return { title: 'Write', icon: <IconWrite size={14} /> }
    case 'grep':
      return { title: 'Grep', icon: <IconGrep size={14} /> }
    case 'glob':
      return { title: 'Glob', icon: <IconGlob size={14} /> }
    case 'ls':
      return { title: 'Ls', icon: <IconFolder size={14} /> }
    case 'edit':
      return { title: 'Edit', icon: <IconEdit size={14} /> }
    case 'todo_write':
      return { title: t('todoTitle'), icon: <IconTodo size={14} /> }
    default:
      return { title: name, icon: <IconTool size={14} /> }
  }
}
