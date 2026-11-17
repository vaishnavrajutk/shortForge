import { useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { convertFileSrc, invoke as tauriInvoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Film, FolderOpen, Plus, Scissors, Sparkles, Download, Mic2, Music2, Clapperboard, Trash2, Copy, Pencil, RefreshCw, Type } from "lucide-react";
import { formatDuration } from "./media-utils.js";

type Project = { id: string; name: string; created_at: string; source_count?: number; shorts_ready?: number; cover_media_path?: string | null };
type Probe = { path: string; file_name: string; duration_seconds: number; width: number; height: number; frame_rate: string; size_bytes: number; has_audio: boolean; category: string; missing: boolean; thumbnail_path: string | null };
type TimelineClip = { id: string; media_path: string; file_name: string; source_in_frame: number; source_out_frame: number; frame_rate: string; duration_seconds: number; has_audio: boolean };
type AudioAsset = { id: string; path: string; file_name: string; duration_seconds: number; file_size_bytes: number };
type TimelineAudioClip = { id: string; audio_asset_id: string; path: string; file_name: string; track_kind: "voice"|"music"|"effects"; timeline_start_seconds: number; source_in_seconds: number; duration_seconds: number; gain: number; fade_in_seconds: number; fade_out_seconds: number; source_duration_seconds: number };
type SceneCut = { id: string; time_seconds: number; score: number; threshold: number };
type AutoHighlight = { id: string; start_seconds: number; end_seconds: number; title: string; reason: string; score: number; output_path: string | null; language: string; description: string; hashtags: string };
type SilenceRange = { id: string; start_seconds: number; end_seconds: number; noise_threshold_db: number; minimum_duration_seconds: number };
type MainPage = "studio"|"projects";
const WEB_API_BASE="http://127.0.0.1:1421";
const LOCAL_HOST_UNAVAILABLE="Local host offline — Projects, media, and rendering are unavailable.";
function fetchLocal(input:RequestInfo|URL,init?:RequestInit):Promise<Response>{
  return fetch(input,init).catch(error=>{
    if(error instanceof TypeError)throw new Error(LOCAL_HOST_UNAVAILABLE);
    throw error;
  });
}
type ClipMode="auto"|"speech"|"gameplay";
const CLIP_LENGTHS={short:[10,30],medium:[15,60],long:[30,90]} as const;
type ClipLengthKey=keyof typeof CLIP_LENGTHS;
function readPreference(key:string):string|null{
  try{return window.localStorage.getItem(key)}catch{return null}
}
function writePreference(key:string,value:string):void{
  try{window.localStorage.setItem(key,value)}catch{return}
}

function invoke<T=unknown>(command:string,args:Record<string,unknown>={}):Promise<T>{
  if(isTauri())return tauriInvoke<T>(command,args);
  return fetchLocal(`${WEB_API_BASE}/api/invoke`,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify({command,args})})
    .then(async response=>{
      const result=await response.json() as {ok:boolean;data?:T;error?:string};
      if(!response.ok||!result.ok)throw new Error(result.error??`Local API returned ${response.status}`);
      return result.data as T;
    });
}

async function chooseLocalFile(extensions:string[]):Promise<string|File|null>{
  if(isTauri()){
    const path=await open({multiple:false,filters:[{name:"Local file",extensions}]});
    return typeof path==="string"?path:null;
  }
  return new Promise(resolve=>{
    const input=document.createElement("input");input.type="file";input.accept=extensions.map(extension=>`.${extension}`).join(",");
    input.onchange=()=>resolve(input.files?.[0]??null);input.oncancel=()=>resolve(null);input.click();
  });
}

async function uploadBrowserFile<T>(file:File,kind:"video"|"audio"|"aux"|"relink",projectId?:string,mediaPath?:string):Promise<T>{
  const url=new URL(`${WEB_API_BASE}/api/upload`);url.searchParams.set("kind",kind);if(projectId)url.searchParams.set("projectId",projectId);if(mediaPath)url.searchParams.set("mediaPath",mediaPath);
  const extension=file.name.split(".").pop()??"bin";
  const response=await fetchLocal(url,{method:"POST",headers:{"Content-Type":"application/octet-stream","X-File-Extension":extension,"X-File-Name":encodeURIComponent(file.name)},body:file});
  const result=await response.json() as {ok:boolean;data?:T;error?:string};
  if(!response.ok||!result.ok)throw new Error(result.error??`Local upload returned ${response.status}`);
  return result.data as T;
}

/** Seconds of work per second of media, learned from this computer's last runs (starts from a cautious guess). */
function learnedRate(key:"listen"|"render",fallback:number):number{
  const stored=Number(readPreference(`shortforge.rate.${key}`));
  return Number.isFinite(stored)&&stored>0?stored:fallback;
}
function learnRate(key:"listen"|"render",measured:number):void{
  if(!Number.isFinite(measured)||measured<=0)return;
  const previous=Number(readPreference(`shortforge.rate.${key}`));
  const next=Number.isFinite(previous)&&previous>0?previous*0.4+measured*0.6:measured;
  writePreference(`shortforge.rate.${key}`,String(next));
}
function languageName(code:string):string{
  try{return new Intl.DisplayNames(undefined,{type:"language"}).of(code)??code;}catch{return code;}
}
function timeLeftLabel(elapsed:number,total:number|null):string{
  if(total===null)return "";
  const left=total-elapsed;
  if(left<=-15)return "taking longer than usual";
  if(left<45)return "almost done";
  if(left<90)return "about 1 min left";
  return `about ${Math.round(left/60)} min left`;
}
function mediaSource(path:string):string{
  return isTauri()?convertFileSrc(path):`${WEB_API_BASE}/api/media?path=${encodeURIComponent(path)}`;
}

function frameRateParts(value:string):[number,number]{
  const [numerator,denominator="1"]=value.split("/");
  const n=Number(numerator),d=Number(denominator);
  return Number.isFinite(n)&&n>0&&Number.isFinite(d)&&d>0?[n,d]:[30,1];
}
function sourceFrameCount(item:Pick<Probe,"frame_rate"|"duration_seconds">):number{
  const [numerator,denominator]=frameRateParts(item.frame_rate);
  return Math.max(1,Math.ceil(item.duration_seconds*numerator/denominator));
}
function clipDuration(clip:Pick<TimelineClip,"source_in_frame"|"source_out_frame"|"frame_rate">):number{
  const [numerator,denominator]=frameRateParts(clip.frame_rate);
  return Math.max(0,clip.source_out_frame-clip.source_in_frame)*denominator/numerator;
}

export default function App() {
  const [mainPage,setMainPage]=useState<MainPage>("studio");
  const [linkUrl,setLinkUrl]=useState("");
  const [inspectorTab,setInspectorTab]=useState<"clips"|"sound"|"export">("clips");
  const [monitorMode,setMonitorMode]=useState<"source"|"program">("source");
  const [projects,setProjects]=useState<Project[]>([]);
  const [active,setActive]=useState<Project|null>(null);
  const [showProjectDialog,setShowProjectDialog]=useState(false);
  const [projectName,setProjectName]=useState("");
  const [media,setMedia]=useState<Probe|null>(null);
  const [assets,setAssets]=useState<Probe[]>([]);
  const [timeline,setTimeline]=useState<TimelineClip[]>([]);
  const [timelineProjectId,setTimelineProjectId]=useState<string|null>(null);
  const [timelineSaveStatus,setTimelineSaveStatus]=useState("Saved");
  const [undoStack,setUndoStack]=useState<TimelineClip[][]>([]);
  const [redoStack,setRedoStack]=useState<TimelineClip[][]>([]);
  const [selectedClipId,setSelectedClipId]=useState<string|null>(null);
  const [audioAssets,setAudioAssets]=useState<AudioAsset[]>([]);
  const [audioTimeline,setAudioTimeline]=useState<TimelineAudioClip[]>([]);
  const [audioTimelineProjectId,setAudioTimelineProjectId]=useState<string|null>(null);
  const [selectedAudioClipId,setSelectedAudioClipId]=useState<string|null>(null);
  const [sceneCuts,setSceneCuts]=useState<SceneCut[]>([]);
  const [silenceRanges,setSilenceRanges]=useState<SilenceRange[]>([]);
  const [silenceNoiseDb,setSilenceNoiseDb]=useState(-35);
  const [minimumSilenceSeconds,setMinimumSilenceSeconds]=useState(0.6);
  const [silenceScanning,setSilenceScanning]=useState(false);
  const [sceneThreshold,setSceneThreshold]=useState(()=>{
    const stored=Number(readPreference("shortforge.sceneThreshold"));
    return Number.isFinite(stored)&&stored>=0.01&&stored<=0.9?stored:0.25;
  });
  const [sceneScanning,setSceneScanning]=useState(false);
  const [highlights,setHighlights]=useState<AutoHighlight[]>([]);
  const [highlightCount,setHighlightCount]=useState(5);
  const [spokenLanguage,setSpokenLanguage]=useState(()=>readPreference("shortforge.spokenLanguage")??"");
  const [clipMode,setClipMode]=useState<ClipMode>(()=>{const saved=readPreference("shortforge.clipMode");return saved==="speech"||saved==="gameplay"?saved:"auto";});
  const [gameName,setGameName]=useState(()=>readPreference("shortforge.gameName")??"");
  const [clipLength,setClipLength]=useState<ClipLengthKey>(()=>{const saved=readPreference("shortforge.clipLength");return saved==="short"||saved==="long"?saved:"medium";});
  const [editingPost,setEditingPost]=useState<{id:string;title:string;description:string;hashtags:string}|null>(null);
  const [highlightScanning,setHighlightScanning]=useState(false);
  const [mode,setMode]=useState<"auto"|"manual">(()=>readPreference("shortforge.mode")==="manual"?"manual":"auto");
  const [phase,setPhase]=useState<"idle"|"fetch"|"listen"|"render">("idle");
  const [fetchKind,setFetchKind]=useState<"link"|"file">("file");
  const [renderCount,setRenderCount]=useState({done:0,total:0});
  const [runningProjectId,setRunningProjectId]=useState<string|null>(null);
  const [projectSearch,setProjectSearch]=useState("");
  const [projectSort,setProjectSort]=useState<"recent"|"name"|"shorts">("recent");
  const [projectCovers,setProjectCovers]=useState<Record<string,string>>({});
  const [selecting,setSelecting]=useState<"shorts"|"projects"|null>(null);
  const [selectedIds,setSelectedIds]=useState<Set<string>>(()=>new Set());
  function toggleSelected(id:string){setSelectedIds(current=>{const next=new Set(current);if(next.has(id))next.delete(id);else next.add(id);return next;});}
  function stopSelecting(){setSelecting(null);setSelectedIds(new Set());}
  type ConfirmRequest={title:string;body:string;action:string};
  const [confirmRequest,setConfirmRequest]=useState<ConfirmRequest|null>(null);
  const confirmResolveRef=useRef<((ok:boolean)=>void)|null>(null);
  function askConfirm(request:ConfirmRequest):Promise<boolean>{
    confirmResolveRef.current?.(false);
    setConfirmRequest(request);
    return new Promise(resolve=>{confirmResolveRef.current=resolve;});
  }
  function answerConfirm(ok:boolean){
    confirmResolveRef.current?.(ok);confirmResolveRef.current=null;setConfirmRequest(null);
  }
  const [stepTiming,setStepTiming]=useState<{start:number;total:number|null}>({start:0,total:null});
  const [runStartedAt,setRunStartedAt]=useState<number|null>(null);
  const [clock,setClock]=useState(()=>Date.now());
  useEffect(()=>{
    if(phase==="idle"){setRunStartedAt(null);return;}
    setClock(Date.now());
    const timer=window.setInterval(()=>setClock(Date.now()),1000);
    return ()=>window.clearInterval(timer);
  },[phase]);
  const [renderingClipId,setRenderingClipId]=useState<string|null>(null);
  const pendingSeekRef=useRef<number|null>(null);
  const autoRunRef=useRef(false);
  const stopRequestedRef=useRef(false);
  const [playheadSeconds,setPlayheadSeconds]=useState(0);
  const [programPlaying,setProgramPlaying]=useState(false);
  const programVideoRef=useRef<HTMLVideoElement|null>(null);
  const sourceVideoRef=useRef<HTMLVideoElement|null>(null);
  const [thumbnailUrls,setThumbnailUrls]=useState<Record<string,string>>({});
  const [start,setStart]=useState("0");
  const [duration,setDuration]=useState("30");
  const [ratio,setRatio]=useState(()=>{
    const stored=readPreference("shortforge.aspectRatio");
    return stored==="1:1"||stored==="16:9"?stored:"9:16";
  });
  const [voice,setVoice]=useState("");
  const [narration,setNarration]=useState("");
  const [srt,setSrt]=useState("");
  const [music,setMusic]=useState("");
  const [busy,setBusy]=useState(false);
  const [rendering,setRendering]=useState(false);
  const [renderProgress,setRenderProgress]=useState(0);
  const [status,setStatus]=useState("Ready — all processing stays on this computer.");
  const [toast,setToast]=useState<{id:number;text:string;error:boolean}|null>(null);
  useEffect(()=>{
    if(status===LOCAL_HOST_UNAVAILABLE||status==="Ready — all processing stays on this computer.")return;
    const error=/error|failed|could not|couldn't|not found|unavailable|refus/i.test(status);
    const id=Date.now();
    setToast({id,text:status.replace(/^Error:\s*/,""),error});
    const timer=window.setTimeout(()=>setToast(current=>current?.id===id?null:current),error?8000:4500);
    return ()=>window.clearTimeout(timer);
  },[status]);
  // A job left running by an earlier page load can no longer be followed from here: end it.
  // A job started before a reload keeps running on this computer and saves its results; follow it.
  const [backgroundJob,setBackgroundJob]=useState(false);
  useEffect(()=>{
    if(highlightScanning||busy||rendering)return;
    let stopped=false;
    const check=async()=>{
      const running=await invoke<boolean>("is_job_running").catch(()=>false);
      if(stopped)return;
      setBackgroundJob(was=>{
        if(was&&!running&&active&&media)void invoke<AutoHighlight[]>("list_auto_highlights",{projectId:active.id,mediaPath:media.path}).then(setHighlights).catch(()=>undefined);
        return running;
      });
    };
    void check();
    const timer=window.setInterval(()=>void check(),5000);
    return ()=>{stopped=true;window.clearInterval(timer);};
  },[highlightScanning,busy,rendering,active?.id,media?.path]);
  const [exportPath,setExportPath]=useState("");

  async function refresh(){
    try {
      if(!isTauri()){
        const response=await fetchLocal(`${WEB_API_BASE}/api/health`);
        if(!response.ok)throw new Error(`Local host returned ${response.status}`);
        const health=await response.json() as {ok:boolean};
        if(!health.ok)throw new Error("Local host health check failed");
      }
      setProjects(await invoke<Project[]>("list_projects"));
      setStatus(current=>current===LOCAL_HOST_UNAVAILABLE?"Local processing host connected.":current);
    } catch(error) {
      setStatus(error instanceof Error&&error.message===LOCAL_HOST_UNAVAILABLE
        ? LOCAL_HOST_UNAVAILABLE
        : String(error));
    }
  }
  useEffect(()=>{void refresh()},[]);
  useEffect(()=>{writePreference("shortforge.aspectRatio",ratio)},[ratio]);
  useEffect(()=>{writePreference("shortforge.sceneThreshold",String(sceneThreshold))},[sceneThreshold]);
  useEffect(()=>{writePreference("shortforge.mode",mode)},[mode]);
  useEffect(()=>{writePreference("shortforge.spokenLanguage",spokenLanguage)},[spokenLanguage]);
  useEffect(()=>{writePreference("shortforge.clipMode",clipMode)},[clipMode]);
  useEffect(()=>{writePreference("shortforge.gameName",gameName)},[gameName]);
  useEffect(()=>{writePreference("shortforge.clipLength",clipLength)},[clipLength]);
  useEffect(()=>{
    if(!isTauri())return;
    let unlisten:(()=>void)|undefined;
    let mounted=true;
    void listen<number>("render-progress",event=>{if(mounted)setRenderProgress(Math.round(event.payload*100))}).then(stop=>{if(mounted)unlisten=stop;else stop()});
    return ()=>{mounted=false;unlisten?.()};
  },[]);
  useEffect(()=>{
    setAssets([]);
    setMedia(null);
    setTimelineProjectId(null);
    setTimeline([]);
    setSelectedClipId(null);
    setAudioAssets([]);
    setAudioTimeline([]);
    setAudioTimelineProjectId(null);
    setSelectedAudioClipId(null);
    setPlayheadSeconds(0);
    setProgramPlaying(false);
    if(!active)return;
    void invoke<Probe[]>("list_project_media",{projectId:active.id}).then(items=>{setAssets(items);setMedia(items[0]??null)}).catch(e=>setStatus(String(e)));
  },[active?.id]);
  useEffect(()=>{
    setSceneCuts([]);
    if(!active||!media)return;
    let mounted=true;
    void invoke<SceneCut[]>("list_scene_cuts",{projectId:active.id,mediaPath:media.path})
      .then(cuts=>{if(mounted)setSceneCuts(cuts)})
      .catch(error=>{if(mounted)setStatus(String(error))});
    return ()=>{mounted=false};
  },[active?.id,media?.path]);
  useEffect(()=>{
    setHighlights([]);
    if(!active||!media)return;
    let mounted=true;
    void invoke<AutoHighlight[]>("list_auto_highlights",{projectId:active.id,mediaPath:media.path})
      .then(items=>{if(mounted)setHighlights(items)})
      .catch(error=>{if(mounted)setStatus(String(error))});
    return ()=>{mounted=false};
  },[active?.id,media?.path]);
  useEffect(()=>{
    setSilenceRanges([]);
    if(!active||!media)return;
    let mounted=true;
    void invoke<SilenceRange[]>("list_silence_ranges",{projectId:active.id,mediaPath:media.path})
      .then(ranges=>{if(mounted)setSilenceRanges(ranges)})
      .catch(error=>{if(mounted)setStatus(String(error))});
    return ()=>{mounted=false};
  },[active?.id,media?.path]);
  useEffect(()=>{
    if(!active)return;
    let mounted=true;
    setTimelineProjectId(null);
    void invoke<TimelineClip[]>("list_timeline",{projectId:active.id}).then(items=>{
      if(!mounted)return;
      setTimeline(items);setUndoStack([]);setRedoStack([]);setSelectedClipId(items[0]?.id??null);setTimelineProjectId(active.id);setTimelineSaveStatus("Saved");
    }).catch(error=>{if(mounted)setStatus(String(error));});
    return ()=>{mounted=false;};
  },[active?.id]);
  useEffect(()=>{
    if(!active)return;
    let mounted=true;
    setAudioTimelineProjectId(null);
    void Promise.all([
      invoke<AudioAsset[]>("list_project_audio",{projectId:active.id}),
      invoke<TimelineAudioClip[]>("list_timeline_audio",{projectId:active.id})
    ]).then(([library,clips])=>{
      if(!mounted)return;
      setAudioAssets(library);setAudioTimeline(clips);setSelectedAudioClipId(clips[0]?.id??null);setAudioTimelineProjectId(active.id);
    }).catch(error=>{if(mounted)setStatus(String(error));});
    return ()=>{mounted=false;};
  },[active?.id]);
  useEffect(()=>{
    if(!active||timelineProjectId!==active.id)return;
    setTimelineSaveStatus("Saving…");
    const timer=window.setTimeout(()=>{
      void invoke("save_timeline",{projectId:active.id,clips:timeline.map(clip=>({id:clip.id,mediaPath:clip.media_path,sourceInFrame:clip.source_in_frame,sourceOutFrame:clip.source_out_frame}))})
        .then(()=>setTimelineSaveStatus("Saved"))
        .catch(error=>{setTimelineSaveStatus("Save failed");setStatus(String(error));});
    },400);
    return ()=>window.clearTimeout(timer);
  },[active?.id,timeline,timelineProjectId]);
  useEffect(()=>{
    if(!active||audioTimelineProjectId!==active.id)return;
    const timer=window.setTimeout(()=>{
      void invoke("save_timeline_audio",{projectId:active.id,clips:audioTimeline.map(clip=>({id:clip.id,audioAssetId:clip.audio_asset_id,trackKind:clip.track_kind,timelineStartSeconds:clip.timeline_start_seconds,sourceInSeconds:clip.source_in_seconds,durationSeconds:clip.duration_seconds,gain:clip.gain,fadeInSeconds:clip.fade_in_seconds,fadeOutSeconds:clip.fade_out_seconds}))})
        .catch(error=>setStatus(String(error)));
    },400);
    return ()=>window.clearTimeout(timer);
  },[active?.id,audioTimeline,audioTimelineProjectId]);
  useEffect(()=>{
    let mounted=true;
    void Promise.all(projects.filter(project=>project.cover_media_path).map(async project=>{
      try{
        const bytes=await invoke<number[]>("get_media_thumbnail",{projectId:project.id,mediaPath:project.cover_media_path});
        let binary="";
        for(const byte of bytes)binary+=String.fromCharCode(byte);
        return [project.id,`data:image/jpeg;base64,${btoa(binary)}` as string] as const;
      }catch{return null;}
    })).then(entries=>{if(mounted)setProjectCovers(Object.fromEntries(entries.filter((entry):entry is readonly [string,string]=>entry!==null)));});
    return ()=>{mounted=false;};
  },[projects]);
  useEffect(()=>{
    let mounted=true;
    void Promise.all(assets.map(async item=>{
      if(item.missing||!item.thumbnail_path||!active)return null;
      try{
        const bytes=await invoke<number[]>("get_media_thumbnail",{projectId:active.id,mediaPath:item.path});
        let binary="";
        for(const byte of bytes)binary+=String.fromCharCode(byte);
        const url=`data:image/jpeg;base64,${btoa(binary)}`;
        return [item.path,url] as const;
      }catch{return null;}
    })).then(entries=>{
      if(mounted)setThumbnailUrls(Object.fromEntries(entries.filter((entry):entry is readonly [string,string]=>entry!==null)));
    });
    return ()=>{mounted=false;};
  },[assets,active?.id]);
  const selectedDuration=useMemo(()=>Number(duration)||0,[duration]);
  const timelineDuration=useMemo(()=>timeline.reduce((sum,clip)=>sum+clipDuration(clip),0),[timeline]);
  const selectedClip=timeline.find(clip=>clip.id===selectedClipId)??null;
  const sceneRanges=useMemo(()=>{
    if(!media||media.duration_seconds<=0)return [];
    const boundaries=[0,...sceneCuts.map(cut=>cut.time_seconds),media.duration_seconds]
      .filter(time=>Number.isFinite(time)&&time>=0&&time<=media.duration_seconds)
      .sort((a,b)=>a-b)
      .filter((time,index,list)=>index===0||time-list[index-1]>0.0001);
    const [numerator,denominator]=frameRateParts(media.frame_rate);
    const minimumDuration=denominator/numerator;
    return boundaries.slice(0,-1).flatMap((startSeconds,index)=>{
      const endSeconds=boundaries[index+1];
      if(endSeconds-startSeconds<minimumDuration)return [];
      const cut=sceneCuts.find(item=>Math.abs(item.time_seconds-startSeconds)<0.0001);
      return [{startSeconds,endSeconds,score:cut?.score??null}];
    });
  },[media,sceneCuts]);
  const programPosition=useMemo(()=>{
    if(!timeline.length)return null;
    let startSeconds=0;
    for(let index=0;index<timeline.length;index++){
      const clip=timeline[index],length=clipDuration(clip);
      if(playheadSeconds<startSeconds+length||index===timeline.length-1){
        return {clip,index,startSeconds,localSeconds:Math.max(0,playheadSeconds-startSeconds)};
      }
      startSeconds+=length;
    }
    return null;
  },[timeline,playheadSeconds]);
  function commitTimeline(next:TimelineClip[]){
    setUndoStack(history=>[...history.slice(-59),timeline]);
    setRedoStack([]);
    setTimeline(next);
  }
  function addSelectedClip(){
    if(!media||media.missing)return;
    const clip:TimelineClip={id:crypto.randomUUID(),media_path:media.path,file_name:media.file_name,source_in_frame:0,source_out_frame:sourceFrameCount(media),frame_rate:media.frame_rate,duration_seconds:media.duration_seconds,has_audio:media.has_audio};
    commitTimeline([...timeline,clip]);setSelectedClipId(clip.id);
  }
  async function findSceneCuts(){
    if(!active||!media||media.missing||sceneScanning)return;
    setSceneScanning(true);setStatus("Scanning local footage for visual scene changes. This can take a while for long files.");
    try{
      const cuts=await invoke<SceneCut[]>("detect_scene_cuts",{projectId:active.id,mediaPath:media.path,threshold:sceneThreshold});
      setSceneCuts(cuts);setStatus(cuts.length?`Found ${cuts.length} scene cut candidates. Review each before adding a shot.`:"No scene cut candidates found at this threshold.");
    }catch(error){setStatus(String(error))}finally{setSceneScanning(false)}
  }
  function clipperOptions(){
    const [minSeconds,maxSeconds]=CLIP_LENGTHS[clipLength];
    return {language:clipMode==="gameplay"?null:spokenLanguage||null,mode:clipMode,minSeconds,maxSeconds,game:clipMode==="speech"?null:gameName.trim()||null};
  }
  async function findHighlights(){
    if(!active||!media||media.missing||highlightScanning)return;
    setHighlightScanning(true);setStatus(clipMode==="gameplay"?"Scanning game sound and on-screen action for highlights. Long sessions can take several minutes.":"Transcribing locally with whisper.cpp and scoring clips. Long videos can take several minutes.");
    try{
      const items=await invoke<AutoHighlight[]>("find_auto_highlights",{projectId:active.id,mediaPath:media.path,maxClips:highlightCount,...clipperOptions()});
      setHighlights(items);setStatus(items.length?`Found ${items.length} clip ideas. Preview each and add the ones you like.`:"No clip-length moments found in this source.");
    }catch(error){setStatus(String(error))}finally{setHighlightScanning(false)}
  }
  async function findSilence(){
    if(!active||!media||media.missing||silenceScanning)return;
    setSilenceScanning(true);setStatus("Analyzing the local audio track for quiet ranges.");
    try{
      const ranges=await invoke<SilenceRange[]>("detect_silence_ranges",{projectId:active.id,mediaPath:media.path,noiseThresholdDb:silenceNoiseDb,minimumDurationSeconds:minimumSilenceSeconds});
      setSilenceRanges(ranges);setStatus(ranges.length?`Found ${ranges.length} quiet ranges. Review them before trimming.`:"No quiet ranges matched those settings.");
    }catch(error){setStatus(String(error))}finally{setSilenceScanning(false)}
  }
  function trimSilenceFromSelected(range:SilenceRange){
    if(!selectedClip||!media||selectedClip.media_path!==media.path)return;
    const [numerator,denominator]=frameRateParts(selectedClip.frame_rate);
    const startFrame=Math.max(selectedClip.source_in_frame,Math.round(range.start_seconds*numerator/denominator));
    const endFrame=Math.min(selectedClip.source_out_frame,Math.round(range.end_seconds*numerator/denominator));
    if(endFrame<=startFrame){setStatus("This quiet range does not overlap the selected clip.");return;}
    const replacement:TimelineClip[]=[];
    if(startFrame>selectedClip.source_in_frame)replacement.push({...selectedClip,id:crypto.randomUUID(),source_out_frame:startFrame});
    if(endFrame<selectedClip.source_out_frame)replacement.push({...selectedClip,id:crypto.randomUUID(),source_in_frame:endFrame});
    if(timeline.length-1+replacement.length>300){setStatus("Removing this range would exceed the 300 clip timeline limit.");return;}
    const index=timeline.findIndex(clip=>clip.id===selectedClip.id);
    const next=[...timeline.slice(0,index),...replacement,...timeline.slice(index+1)];
    commitTimeline(next);setSelectedClipId(replacement[0]?.id??next[Math.min(index,next.length-1)]?.id??null);
    setStatus(`Removed ${formatDuration((endFrame-startFrame)*denominator/numerator)} of quiet audio from the selected clip. Undo is available.`);
  }
  function previewSceneRange(timeSeconds:number){
    const target=Math.max(0,Math.min(timeSeconds,Math.max(0,media?.duration_seconds??0.001)));
    const video=sourceVideoRef.current;
    if(video&&video.readyState>=1&&monitorMode==="source"){video.currentTime=target;void video.play().catch(()=>undefined);}
    else pendingSeekRef.current=target;
  }
  function applyPendingSeek(video:HTMLVideoElement){
    if(pendingSeekRef.current===null)return;
    video.currentTime=pendingSeekRef.current;pendingSeekRef.current=null;void video.play().catch(()=>undefined);
  }
  async function renderClip(projectId:string,item:AutoHighlight){
    setRenderingClipId(item.id);
    try{const done=await invoke<AutoHighlight>("render_auto_highlight",{projectId,highlightId:item.id});setHighlights(items=>items.map(x=>x.id===done.id?done:x));return true;}
    catch(e){setStatus(String(e));return false;}
    finally{setRenderingClipId(null);}
  }
  async function runAuto(projectId:string,source:Probe){
    if(autoRunRef.current)return;
    if(!source.has_audio&&clipMode==="speech"){setStatus("Speech mode needs a video with sound. Switch to Gameplay to clip this source.");return;}
    autoRunRef.current=true;stopRequestedRef.current=false;setHighlightScanning(true);setRunningProjectId(projectId);
    try{
      const listenStart=Date.now();
      setRunStartedAt(started=>started??listenStart);
      setStepTiming({start:listenStart,total:source.duration_seconds*learnedRate("listen",0.9)});
      setPhase("listen");
      const findPicks=()=>invoke<AutoHighlight[]>("find_auto_highlights",{projectId,mediaPath:source.path,maxClips:highlightCount,...clipperOptions()});
      const picks=await findPicks().catch(error=>{
        if(String(error).includes("Another job is already running"))setBackgroundJob(true);
        throw error;
      });
      learnRate("listen",(Date.now()-listenStart)/1000/Math.max(1,source.duration_seconds));
      setHighlights(picks);
      const todo=picks.filter(pick=>!pick.output_path);
      if(!todo.length){setStatus(picks.length?"No new moments found that don't overlap your finished shorts.":"No clip-length moments found in this video.");return;}
      let ready=0;
      const renderStart=Date.now();
      for(const [index,item] of todo.entries()){
        if(stopRequestedRef.current)break;
        const clipStart=Date.now();
        const secondsLeft=todo.slice(index).reduce((sum,pick)=>sum+pick.end_seconds-pick.start_seconds,0);
        setStepTiming({start:renderStart,total:(clipStart-renderStart)/1000+secondsLeft*learnedRate("render",1.2)});
        setPhase("render");setRenderCount({done:index,total:todo.length});
        if(await renderClip(projectId,item)){ready++;learnRate("render",(Date.now()-clipStart)/1000/Math.max(1,item.end_seconds-item.start_seconds));}
        setRenderCount({done:index+1,total:todo.length});
      }
      setStatus(stopRequestedRef.current?`Stopped. ${ready} new ${ready===1?"short is":"shorts are"} ready.`:`${ready} new ${ready===1?"short":"shorts"} ready to download.`);
    }catch(e){setStatus(stopRequestedRef.current||String(e).includes("Stopped.")?"Stopped. Finished shorts were kept.":String(e));}
    finally{autoRunRef.current=false;setHighlightScanning(false);setPhase("idle");setRunningProjectId(null);void refresh();}
  }
  async function stopAuto(){
    stopRequestedRef.current=true;setBackgroundJob(false);
    try{await invoke("cancel_render");}catch{/* nothing was running between steps */}
  }
  function editInManual(item:AutoHighlight){
    addSceneRange(item.start_seconds,item.end_seconds);setMode("manual");setInspectorTab("clips");
  }
  function addSceneRange(startSeconds:number,endSeconds:number){
    if(!media||media.missing)return;
    if(timeline.length>=300){setStatus("A timeline can contain at most 300 clips.");return;}
    const [numerator,denominator]=frameRateParts(media.frame_rate);
    const frameCount=sourceFrameCount(media);
    const sourceInFrame=Math.max(0,Math.min(frameCount-1,Math.round(startSeconds*numerator/denominator)));
    const sourceOutFrame=Math.max(sourceInFrame+1,Math.min(frameCount,Math.round(endSeconds*numerator/denominator)));
    const clip:TimelineClip={id:crypto.randomUUID(),media_path:media.path,file_name:media.file_name,source_in_frame:sourceInFrame,source_out_frame:sourceOutFrame,frame_rate:media.frame_rate,duration_seconds:media.duration_seconds,has_audio:media.has_audio};
    const next=[...timeline,clip];commitTimeline(next);setSelectedClipId(clip.id);setPlayheadSeconds(timelineDuration);previewSceneRange(startSeconds);setStatus("Reviewed shot added to the end of the timeline.");
  }
  function updateClipFrames(id:string,key:"source_in_frame"|"source_out_frame",value:number){
    commitTimeline(timeline.map(clip=>{
      if(clip.id!==id)return clip;
      const frameCount=sourceFrameCount(clip);
      if(key==="source_in_frame")return {...clip,source_in_frame:Math.max(0,Math.min(Math.trunc(value),clip.source_out_frame-1))};
      return {...clip,source_out_frame:Math.max(clip.source_in_frame+1,Math.min(frameCount,Math.trunc(value)))};
    }));
  }
  function splitClip(id:string){
    const index=timeline.findIndex(clip=>clip.id===id);if(index<0)return;
    const clip=timeline[index];const midpoint=Math.floor((clip.source_in_frame+clip.source_out_frame)/2);
    if(midpoint<=clip.source_in_frame||midpoint>=clip.source_out_frame){setStatus("A clip needs at least two frames to split.");return;}
    const left={...clip,id:crypto.randomUUID(),source_out_frame:midpoint};
    const right={...clip,id:crypto.randomUUID(),source_in_frame:midpoint};
    const next=[...timeline.slice(0,index),left,right,...timeline.slice(index+1)];commitTimeline(next);setSelectedClipId(right.id);
  }
  function moveClip(id:string,delta:-1|1){
    const index=timeline.findIndex(clip=>clip.id===id),target=index+delta;
    if(index<0||target<0||target>=timeline.length)return;
    const next=[...timeline];[next[index],next[target]]=[next[target],next[index]];commitTimeline(next);
  }
  function undoTimeline(){
    if(!undoStack.length)return;
    setRedoStack(history=>[...history,timeline]);setTimeline(undoStack.at(-1)!);setUndoStack(history=>history.slice(0,-1));
  }
  function redoTimeline(){
    if(!redoStack.length)return;
    setUndoStack(history=>[...history,timeline]);setTimeline(redoStack.at(-1)!);setRedoStack(history=>history.slice(0,-1));
  }
  function appendAudioAsset(asset:AudioAsset){
    if(playheadSeconds>=900){setStatus("Audio clips must begin before 900 seconds.");return;}
    const duration=Math.min(asset.duration_seconds,900-playheadSeconds);
    const clip:TimelineAudioClip={id:crypto.randomUUID(),audio_asset_id:asset.id,path:asset.path,file_name:asset.file_name,track_kind:"music",timeline_start_seconds:playheadSeconds,source_in_seconds:0,duration_seconds:duration,gain:0.5,fade_in_seconds:0,fade_out_seconds:0,source_duration_seconds:asset.duration_seconds};
    setAudioTimeline(items=>[...items,clip]);setSelectedAudioClipId(clip.id);
  }
  async function importAudioTrack(){
    if(!active||audioTimelineProjectId!==active.id)return;
    if(playheadSeconds>=900){setStatus("Audio clips must begin before 900 seconds.");return;}
    const selected=await chooseLocalFile(["wav","mp3","m4a","aac","flac","ogg","wma"]);
    if(!selected)return;
    setBusy(true);setStatus("Copying audio into the local project library and reading its duration…");
    try{
      const asset=typeof selected==="string"
        ?await invoke<AudioAsset>("import_audio",{projectId:active.id,path:selected})
        :await uploadBrowserFile<AudioAsset>(selected,"audio",active.id);
      setAudioAssets(items=>[asset,...items]);appendAudioAsset(asset);setStatus("Audio added to the project timeline.");
    }catch(error){setStatus(String(error));}finally{setBusy(false);}
  }
  function updateAudioClip(id:string,patch:Partial<Pick<TimelineAudioClip,"track_kind"|"timeline_start_seconds"|"source_in_seconds"|"duration_seconds"|"gain"|"fade_in_seconds"|"fade_out_seconds">>){
    setAudioTimeline(items=>items.map(clip=>clip.id===id?{...clip,...patch}:clip));
  }
  function selectTimelineClip(clip:TimelineClip,index:number){
    setSelectedClipId(clip.id);
    const item=assets.find(asset=>asset.path===clip.media_path);if(item)setMedia(item);
    setPlayheadSeconds(timeline.slice(0,index).reduce((sum,part)=>sum+clipDuration(part),0));
  }
  function stepPlayhead(direction:-1|1){
    if(!programPosition)return;
    const [numerator,denominator]=frameRateParts(programPosition.clip.frame_rate);
    setPlayheadSeconds(position=>Math.max(0,Math.min(timelineDuration,position+direction*denominator/numerator)));
  }
  function onProgramTimeUpdate(){
    const video=programVideoRef.current,position=programPosition;
    if(!video||!position)return;
    const [numerator,denominator]=frameRateParts(position.clip.frame_rate);
    const sourceInSeconds=position.clip.source_in_frame*denominator/numerator;
    const localSeconds=Math.max(0,video.currentTime-sourceInSeconds);
    if(localSeconds>=clipDuration(position.clip)-1/30){
      if(position.index<timeline.length-1){selectTimelineClip(timeline[position.index+1],position.index+1);setProgramPlaying(true);}
      else{setPlayheadSeconds(timelineDuration);setProgramPlaying(false);}
      return;
    }
    setPlayheadSeconds(position.startSeconds+localSeconds);
  }
  function loadProgramFrame(){
    const video=programVideoRef.current,position=programPosition;
    if(!video||!position)return;
    const [numerator,denominator]=frameRateParts(position.clip.frame_rate);
    const sourceInSeconds=position.clip.source_in_frame*denominator/numerator;
    const target=sourceInSeconds+Math.min(position.localSeconds,Math.max(0,clipDuration(position.clip)-0.001));
    video.currentTime=target;
    if(programPlaying)void video.play().catch(()=>{setProgramPlaying(false);setStatus("The selected source format could not play in the local preview.");});
  }
  useEffect(()=>{
    if(!timelineDuration){setPlayheadSeconds(0);return;}
    setPlayheadSeconds(position=>Math.max(0,Math.min(position,timelineDuration)));
  },[timelineDuration]);
  useEffect(()=>{
    const video=programVideoRef.current,position=programPosition;
    if(!video||!position||video.readyState<1)return;
    const [numerator,denominator]=frameRateParts(position.clip.frame_rate);
    const sourceInSeconds=position.clip.source_in_frame*denominator/numerator;
    const target=sourceInSeconds+Math.min(position.localSeconds,Math.max(0,clipDuration(position.clip)-0.001));
    if(Math.abs(video.currentTime-target)>0.1)video.currentTime=target;
    if(programPlaying&&video.paused)void video.play().catch(()=>setProgramPlaying(false));
    if(!programPlaying&&!video.paused)video.pause();
  },[programPosition,programPlaying]);
  async function newProject(event:FormEvent<HTMLFormElement>){
    event.preventDefault();
    if(!projectName.trim())return;
    try{const p=await invoke<Project>("create_project",{name:projectName.trim()});setActive(p);setMainPage("studio");setProjectName("");setShowProjectDialog(false);await refresh();setStatus("Project created.");}catch(e){setStatus(String(e));}
  }
  async function chooseVideo(){
    const selected=await chooseLocalFile(["mp4","mov","mkv","webm","avi","m4v"]);
    if(!selected)return;
    if(!active){setStatus("Create or select a project before importing media.");return;}
    setBusy(true);setFetchKind("file");setPhase("fetch");setRunStartedAt(Date.now());setStepTiming({start:Date.now(),total:null});setStatus("Copying footage into the local project library and reading metadata…");
    try{const p=typeof selected==="string"?await invoke<Probe>("import_media",{projectId:active.id,path:selected}):await uploadBrowserFile<Probe>(selected,"video",active.id);setMedia(p);setAssets(await invoke<Probe[]>("list_project_media",{projectId:active.id}));setStatus("Media imported into the local project library.");if(mode==="auto")setStatus("Video ready. Choose how many shorts and press Make shorts.");}
    catch(e){setStatus(String(e));}finally{setBusy(false);setPhase(current=>current==="fetch"?"idle":current);}
  }
  async function importLink(projectId:string,url:string){
    setBusy(true);setFetchKind("link");setPhase("fetch");setRunStartedAt(Date.now());setStepTiming({start:Date.now(),total:null});setStatus("Downloading the video from the link, then copying it into the project library. Long videos take a while.");
    stopRequestedRef.current=false;
    try{const p=await invoke<Probe>("import_url",{projectId,url});setMedia(p);setAssets(await invoke<Probe[]>("list_project_media",{projectId}));setLinkUrl("");setMonitorMode("source");setStatus(mode==="auto"?"Video ready. Choose how many shorts and press Make shorts.":"Video downloaded and imported.");}
    catch(e){setStatus(String(e));}finally{setBusy(false);setPhase(current=>current==="fetch"?"idle":current);}
  }
  async function startFromLink(event:FormEvent){
    event.preventDefault();
    const url=linkUrl.trim();if(!url||busy)return;
    setMode("auto");try{const p=await invoke<Project>("create_project",{name:`Short ${new Date().toLocaleString(undefined,{month:"short",day:"numeric",hour:"2-digit",minute:"2-digit"})}`});setActive(p);setMainPage("studio");void refresh();await importLink(p.id,url);}
    catch(e){setStatus(String(e));}
  }
  async function changeCategory(item:Probe,category:string){
    if(!active)return;
    try{await invoke("set_media_category",{projectId:active.id,mediaPath:item.path,category});setAssets(items=>items.map(x=>x.path===item.path?{...x,category}:x));if(media?.path===item.path)setMedia({...item,category});}
    catch(e){setStatus(String(e));}
  }
  async function deleteProject(project:Project){
    if(runningProjectId===project.id){setStatus("Wait for this project to finish making shorts before deleting it.");return;}
    if(!await askConfirm({title:`Delete “${project.name}”?`,body:"Its imported videos, rendered shorts and edits are removed from ShortForge. Files you picked from your computer stay where they are.",action:"Delete project"}))return;
    try{await invoke("delete_project",{projectId:project.id});if(active?.id===project.id){setActive(null);setMedia(null);setAssets([]);setHighlights([]);}await refresh();setStatus(`Deleted “${project.name}”.`);}
    catch(e){setStatus(String(e));}
  }
  async function copyPost(item:AutoHighlight){
    const post=[item.title,item.description,item.hashtags].filter(Boolean).join("\n\n");
    try{await navigator.clipboard.writeText(post);setStatus("Title, description and hashtags copied. Paste them when you upload.");}
    catch{setStatus("Could not copy. Select the text on the card instead.");}
  }
  async function deleteSelectedShorts(){
    if(!active||selectedIds.size===0)return;
    const chosen=highlights.filter(item=>selectedIds.has(item.id));
    if(!await askConfirm({title:`Delete ${chosen.length} ${chosen.length===1?"short":"shorts"}?`,body:"Their rendered video files are removed. This can't be undone.",action:`Delete ${chosen.length}`}))return;
    const removed=new Set<string>();
    for(const item of chosen){try{await invoke("delete_auto_highlight",{projectId:active.id,highlightId:item.id});removed.add(item.id);}catch(e){setStatus(String(e));}}
    setHighlights(items=>items.filter(item=>!removed.has(item.id)));stopSelecting();setStatus(`Deleted ${removed.size} ${removed.size===1?"short":"shorts"}.`);
  }
  async function downloadSelectedShorts(){
    const chosen=highlights.filter(item=>selectedIds.has(item.id)&&item.output_path);
    for(const item of chosen){
      const link=document.createElement("a");
      link.href=isTauri()?mediaSource(item.output_path as string):`${mediaSource(item.output_path as string)}&download=1`;
      link.download="";document.body.appendChild(link);link.click();link.remove();
      await new Promise(resolve=>window.setTimeout(resolve,400));
    }
    setStatus(chosen.length?`Downloading ${chosen.length} ${chosen.length===1?"short":"shorts"}. Your browser may ask to allow multiple downloads.`:"Pick shorts that are rendered to download them.");
  }
  async function deleteSelectedProjects(){
    const chosen=projects.filter(project=>selectedIds.has(project.id)&&project.id!==runningProjectId);
    if(chosen.length===0)return;
    if(!await askConfirm({title:`Delete ${chosen.length} ${chosen.length===1?"project":"projects"}?`,body:"Their imported videos, rendered shorts and edits are removed from ShortForge. Files you picked from your computer stay where they are.",action:`Delete ${chosen.length}`}))return;
    for(const project of chosen){try{await invoke("delete_project",{projectId:project.id});if(active?.id===project.id){setActive(null);setMedia(null);setAssets([]);setHighlights([]);}}catch(e){setStatus(String(e));}}
    stopSelecting();await refresh();setStatus(`Deleted ${chosen.length} ${chosen.length===1?"project":"projects"}.`);
  }
  async function savePost(){
    if(!active||!editingPost)return;
    try{
      const saved=await invoke<AutoHighlight>("update_auto_highlight_text",{projectId:active.id,highlightId:editingPost.id,title:editingPost.title,description:editingPost.description,hashtags:editingPost.hashtags});
      setHighlights(items=>items.map(item=>item.id===saved.id?saved:item));setEditingPost(null);setStatus("Post text saved.");
    }catch(e){setStatus(String(e));}
  }
  async function deleteShort(item:AutoHighlight){
    if(!active)return;
    if(!await askConfirm(item.output_path?{title:`Delete “${item.title}”?`,body:"The rendered video file is removed. This can't be undone.",action:"Delete short"}:{title:`Remove “${item.title}”?`,body:"This suggestion is removed from the list.",action:"Remove"}))return;
    try{await invoke("delete_auto_highlight",{projectId:active.id,highlightId:item.id});setHighlights(items=>items.filter(x=>x.id!==item.id));}
    catch(e){setStatus(String(e));}
  }
  async function deleteAsset(item:Probe){
    if(!active)return;
    const confirmed=await askConfirm({title:`Remove “${item.file_name}”?`,body:"Its ShortForge library copy, thumbnail and timeline clips are deleted. The original file you imported is not changed.",action:"Remove video"});
    if(!confirmed)return;
    setBusy(true);
    try{
      await invoke("delete_media_asset",{projectId:active.id,mediaPath:item.path});
      const remaining=assets.filter(asset=>asset.path!==item.path);
      setAssets(remaining);
      setThumbnailUrls(urls=>{const next={...urls};delete next[item.path];return next;});
      if(media?.path===item.path)setMedia(remaining[0]??null);
      const nextTimeline=timeline.filter(clip=>clip.media_path!==item.path);
      setTimeline(nextTimeline);
      if(!nextTimeline.some(clip=>clip.id===selectedClipId))setSelectedClipId(nextTimeline[0]?.id??null);
      setStatus("Media removed from the project library.");
    }catch(error){setStatus(String(error));}
    finally{setBusy(false);}
  }
  async function relinkAsset(item:Probe){
    if(!active)return;
    const replacement=await chooseLocalFile(["mp4","mov","mkv","webm","avi","m4v"]);
    if(!replacement)return;
    setBusy(true);setStatus("Checking replacement media and copying it into the local library…");
    try{const updated=typeof replacement==="string"?await invoke<Probe>("relink_media",{projectId:active.id,mediaPath:item.path,replacementPath:replacement}):await uploadBrowserFile<Probe>(replacement,"relink",active.id,item.path);const items=await invoke<Probe[]>("list_project_media",{projectId:active.id});setAssets(items);setMedia(updated);setStatus("Media relinked.");}
    catch(e){setStatus(String(e));}finally{setBusy(false);}
  }
  async function pickAux(kind:"voice"|"music"|"srt"){
    const extensions=kind==="srt"?["srt"]:kind==="music"?["mp3","wav","m4a","aac","flac","ogg"]:["wav","mp3","m4a"];
    const selected=await chooseLocalFile(extensions);
    if(!selected)return;
    const p=typeof selected==="string"?selected:(await uploadBrowserFile<{path:string}>(selected,"aux")).path;
    if(kind==="voice")setVoice(p);else if(kind==="music")setMusic(p);else setSrt(p);
  }
  async function exportShort(){
    if(!media){setStatus("Import a source video first.");return;}
    if(!active){setStatus("Create or select a project first.");return;}
    if(timeline.length===0&&selectedDuration<=0){setStatus("Duration must be greater than zero.");return;}
    setBusy(true);setRendering(true);setRenderProgress(0);setStatus(isTauri()?"Rendering with FFmpeg… 0%":"Rendering locally. Browser mode does not show render progress.");
    try{
      let out:string;
      if(timeline.length>0){
        await invoke("save_timeline",{projectId:active.id,clips:timeline.map(clip=>({id:clip.id,mediaPath:clip.media_path,sourceInFrame:clip.source_in_frame,sourceOutFrame:clip.source_out_frame}))});
        out=await invoke<string>("render_timeline",{projectId:active.id,aspectRatio:ratio,voiceoverPath:voice||null,musicPath:music||null,subtitlePath:srt||null});
      }else{
        out=await invoke<string>("render_short",{projectId:active.id,inputPath:media.path,startSeconds:Number(start)||0,durationSeconds:selectedDuration,aspectRatio:ratio,voiceoverPath:voice||null,musicPath:music||null,subtitlePath:srt||null});
      }
      setExportPath(out);setStatus("Export completed and verified.");
    }catch(e){setStatus(String(e));}finally{setBusy(false);setRendering(false);setRenderProgress(0);}
  }
  async function cancelRender(){try{await invoke("cancel_render");setStatus("Cancellation requested…");}catch(e){setStatus(String(e));}}
  async function generateVoice(){
    if(!narration.trim()){setStatus("Enter narration text first.");return;}
    setBusy(true);setStatus("Generating voice-over with installed Windows speech voices…");
    try{const p=await invoke<string>("generate_voiceover",{text:narration,voiceName:null});setVoice(p);setStatus("Voice-over WAV generated.");}
    catch(e){setStatus(String(e));}finally{setBusy(false);}
  }
  const showHome=mainPage==="projects"||!active;
  const mapPercent=(seconds:number)=>`${Math.max(0,Math.min(100,seconds/(media?.duration_seconds||1)*100))}%`;
  const clipMap=media&&media.duration_seconds>0?<div className="clip-map">
    <div className="clip-map-track">
      {silenceRanges.map(range=><span key={range.id} className="map-silence" style={{left:mapPercent(range.start_seconds),width:mapPercent(range.end_seconds-range.start_seconds)}}/>)}
      {highlights.map(item=><button key={item.id} className="map-pick" style={{left:mapPercent(item.start_seconds),width:mapPercent(item.end_seconds-item.start_seconds),opacity:0.45+0.55*item.score}} onClick={()=>{setMonitorMode("source");previewSceneRange(item.start_seconds)}} title={`${item.title} · ${formatDuration(item.start_seconds)} – ${formatDuration(item.end_seconds)}`} aria-label={`Preview clip idea ${item.title}`}/>)}
      {sceneCuts.map(cut=><span key={cut.id} className="map-cut" style={{left:mapPercent(cut.time_seconds)}}/>)}
    </div>
    <div className="clip-map-legend"><span><i className="swatch-pick"/>Clip ideas {highlights.length}</span><span><i className="swatch-cut"/>Scene cuts {sceneCuts.length}</span><span><i className="swatch-silence"/>Quiet ranges {silenceRanges.length}</span><span className="clip-map-end num">{media.file_name} · {formatDuration(media.duration_seconds)}</span></div>
  </div>:null;
  const sourceView=media&&!media.missing?<video ref={sourceVideoRef} key={media.path} className="source-video" src={mediaSource(media.path)} controls playsInline preload="metadata" onLoadedMetadata={event=>applyPendingSeek(event.currentTarget)} onError={()=>setStatus("The selected source format could not play in the local preview.")}/>:<div className="monitor-empty"><div className="preview-empty-content"><div className="preview-empty-icon"><Film size={36}/></div><b>{media?.missing?"Source file unavailable":"Import a long video to start"}</b><span>{media?.missing?"Relink the missing file from the media list.":"MP4, MOV, MKV, WEBM or AVI from this computer."}</span>{!media&&<div className="inline-actions"><button className="primary-btn" onClick={()=>void chooseVideo()}><FolderOpen size={16}/> Import video</button></div>}</div></div>;
  const programView=<div className="program-view"><div className={`preview-frame ${ratio==="1:1"?"square":ratio==="16:9"?"wide":""}`}>{programPosition?<video key={programPosition.clip.id} ref={programVideoRef} className="program-video" src={mediaSource(programPosition.clip.media_path)} playsInline preload="metadata" onLoadedMetadata={loadProgramFrame} onTimeUpdate={onProgramTimeUpdate} onError={()=>setStatus("The selected source format could not play in the local preview.")}/>:<div className="preview-placeholder"><Clapperboard size={27}/><b>Timeline is empty</b><span>Add clips from the Clips panel. {ratio} preview.</span></div>}</div>{programPosition&&<div className="program-controls"><div className="program-buttons"><button onClick={()=>stepPlayhead(-1)} aria-label="Step one frame backward">|‹</button><button className="program-play" onClick={()=>{if(programPlaying)setProgramPlaying(false);else{if(playheadSeconds>=timelineDuration)setPlayheadSeconds(0);setProgramPlaying(true);}}}>{programPlaying?"Pause":"Play"}</button><button onClick={()=>stepPlayhead(1)} aria-label="Step one frame forward">›|</button><span>{formatDuration(playheadSeconds)} / {formatDuration(timelineDuration)}</span></div><input aria-label="Program playhead" type="range" min={0} max={Math.max(timelineDuration,0.01)} step={programPosition?frameRateParts(programPosition.clip.frame_rate)[1]/frameRateParts(programPosition.clip.frame_rate)[0]:0.033333} value={Math.min(playheadSeconds,timelineDuration)} onChange={event=>setPlayheadSeconds(Number(event.target.value))}/></div>}</div>;
  const phaseOrder=["fetch","listen","render"] as const;
  const pipeline=phase!=="idle"&&<ol className="pipeline" aria-live="polite">{phaseOrder.map((key,index)=>{
    const state=phaseOrder.indexOf(phase)>index?"done":key===phase?"active":"todo";
    const label=key==="fetch"?(fetchKind==="link"?"Download video":"Copy video"):key==="listen"?"Find the best moments":"Render shorts";
    const elapsed=Math.max(0,(clock-stepTiming.start)/1000);
    const times=[`${formatDuration(elapsed)} elapsed`,timeLeftLabel(elapsed,stepTiming.total)].filter(Boolean).join(" · ");
    const note=state==="todo"?"Waiting":key==="fetch"?(state==="active"?times:"Done"):key==="listen"?(state==="active"?times:`${highlights.length} moments found`):(state==="active"?`${renderCount.done} of ${renderCount.total} ready · ${times}`:"Done");
    return <li key={key} className={`pipeline-step ${state}`}><span className="pipeline-mark" aria-hidden="true"/><div className="pipeline-text"><b>{label}</b><small>{note}</small></div>
      {state==="active"&&key==="fetch"&&<span className="meter indeterminate" aria-hidden="true"><i/></span>}
      {state==="active"&&key==="listen"&&<span className="wave" aria-hidden="true"><i/><i/><i/><i/><i/><i/><i/></span>}
      {state==="active"&&key==="render"&&<span className="meter" role="progressbar" aria-valuemin={0} aria-valuemax={renderCount.total} aria-valuenow={renderCount.done}><i style={{width:`${renderCount.total?renderCount.done/renderCount.total*100:0}%`}}/></span>}
    </li>})}</ol>;
  const skeletonCards=<div className="short-grid" style={{"--n":Math.min(highlightCount,6)} as React.CSSProperties}>{Array.from({length:highlightCount},(_,index)=><div className="short-card skeleton" key={index} style={{"--i":index} as React.CSSProperties} aria-hidden="true"><div className="short-frame"/><div className="short-meta"><span className="skeleton-line"/><span className="skeleton-line narrow"/><span className="skeleton-line"/><span className="skeleton-line narrow"/></div><div className="short-actions"><span className="skeleton-line button"/></div></div>)}</div>;
  const selectionBar=(kind:"shorts"|"projects",allIds:string[])=>selecting===kind?<div className="select-bar" role="toolbar" aria-label="Selection">
    <span className="num">{selectedIds.size} selected</span>
    <button className="text-btn" onClick={()=>setSelectedIds(selectedIds.size===allIds.length?new Set():new Set(allIds))}>{selectedIds.size===allIds.length?"Select none":"Select all"}</button>
    {kind==="shorts"&&<button className="small-btn" onClick={()=>void downloadSelectedShorts()} disabled={selectedIds.size===0}><Download size={14}/> Download</button>}
    <button className="danger-btn small" onClick={()=>void (kind==="shorts"?deleteSelectedShorts():deleteSelectedProjects())} disabled={selectedIds.size===0}><Trash2 size={14}/> Delete</button>
    <button className="quiet-btn" onClick={stopSelecting}>Done</button>
  </div>:allIds.length>1?<button className="small-btn select-toggle" onClick={()=>{setSelecting(kind);setSelectedIds(new Set());}}>Select</button>:null;
  const ratioPicker=<div className="ratio-group" role="group" aria-label="Output format">{["9:16","1:1","16:9"].map(x=><button key={x} className={ratio===x?"ratio active-ratio":"ratio"} onClick={()=>setRatio(x)}>{x}</button>)}</div>;
  return <div className="app-shell">
    {editingPost&&<div className="modal-backdrop" onMouseDown={event=>{if(event.target===event.currentTarget)setEditingPost(null)}} onKeyDown={event=>{if(event.key==="Escape")setEditingPost(null)}}>
      <form className="project-dialog post-dialog" role="dialog" aria-modal="true" aria-labelledby="post-title" onSubmit={event=>{event.preventDefault();void savePost();}}>
        <h2 id="post-title">Edit post text</h2>
        <label>Title<input autoFocus maxLength={150} value={editingPost.title} onChange={event=>setEditingPost({...editingPost,title:event.target.value})}/></label>
        <label>Description<textarea maxLength={2200} rows={5} value={editingPost.description} onChange={event=>setEditingPost({...editingPost,description:event.target.value})}/></label>
        <label>Hashtags<input maxLength={500} value={editingPost.hashtags} onChange={event=>setEditingPost({...editingPost,hashtags:event.target.value})}/></label>
        <div className="dialog-actions"><button type="button" className="quiet-btn" onClick={()=>setEditingPost(null)}>Cancel</button><button type="submit" className="primary-btn" disabled={!editingPost.title.trim()}>Save</button></div>
      </form>
    </div>}
    {confirmRequest&&<div className="modal-backdrop" onMouseDown={event=>{if(event.target===event.currentTarget)answerConfirm(false)}} onKeyDown={event=>{if(event.key==="Escape")answerConfirm(false)}}>
      <div className="project-dialog confirm-dialog" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" aria-describedby="confirm-body">
        <h2 id="confirm-title">{confirmRequest.title}</h2>
        <p id="confirm-body">{confirmRequest.body}</p>
        <div className="dialog-actions"><button type="button" className="quiet-btn" autoFocus onClick={()=>answerConfirm(false)}>Cancel</button><button type="button" className="danger-btn" onClick={()=>answerConfirm(true)}><Trash2 size={15}/> {confirmRequest.action}</button></div>
      </div>
    </div>}
    {showProjectDialog&&<div className="modal-backdrop" onMouseDown={event=>{if(event.target===event.currentTarget)setShowProjectDialog(false)}}><form className="project-dialog" onSubmit={event=>void newProject(event)}><h2>New project</h2><label htmlFor="project-name">Project name<input id="project-name" autoFocus maxLength={120} value={projectName} onChange={event=>setProjectName(event.target.value)} placeholder="My Short" /></label><div className="dialog-actions"><button type="button" className="quiet-btn" onClick={()=>setShowProjectDialog(false)}>Cancel</button><button type="submit" className="primary-btn" disabled={!projectName.trim()||busy}><Plus size={15}/> Create project</button></div></form></div>}
    <header className="topbar">
      <button className="brand" onClick={()=>{setMainPage("projects");void refresh()}} aria-label="ShortForge projects"><svg className="brand-mark" viewBox="0 0 20 28" aria-hidden="true"><rect x="1" y="1" width="18" height="26" rx="4" fill="none" stroke="currentColor" strokeWidth="2"/><rect x="5" y="9" width="10" height="10" rx="1.5" fill="var(--accent)"/></svg><b>ShortForge</b></button>
      <span className="topbar-divider" aria-hidden="true"/>
      <h1>{showHome?"Projects":active.name}</h1>
      {!showHome&&<button className="text-btn" onClick={()=>{setMainPage("projects");void refresh()}}><FolderOpen size={15}/> All projects</button>}
      {!showHome&&<div className="segmented mode-switch" role="tablist" aria-label="Mode">{(["auto","manual"] as const).map(value=><button key={value} role="tab" aria-selected={mode===value} className={mode===value?"current":""} onClick={()=>setMode(value)}>{value==="auto"?"Auto":"Manual"}</button>)}</div>}
      <div className="topbar-end">
        {!showHome&&mode==="manual"&&<span className="topbar-meta">{assets.length} {assets.length===1?"source":"sources"} · {timeline.length} {timeline.length===1?"clip":"clips"} · {formatDuration(timelineDuration)}</span>}
        {showHome?<button className="primary-btn" onClick={()=>setShowProjectDialog(true)}><Plus size={16}/> New project</button>:mode==="manual"?<button className="primary-btn" onClick={()=>setInspectorTab("export")}><Download size={16}/> Export</button>:null}
      </div>
    </header>
    {showHome?<main className="home">
      <form className="home-start" onSubmit={event=>void startFromLink(event)}>
        <h2>Make shorts from a long video</h2>
        <p>Paste a YouTube link or import a file. ShortForge transcribes it on this computer, picks the strongest 15–60 second moments, and renders them as vertical shorts ready to post.</p>
        <div className="home-link"><input type="url" aria-label="Video link" placeholder="https://www.youtube.com/watch?v=…" value={linkUrl} onChange={event=>setLinkUrl(event.target.value)}/><button type="submit" className="primary-btn" disabled={busy||!linkUrl.trim()}>{busy?"Working…":"Start"}</button></div>
        <button type="button" className="text-btn home-file" onClick={()=>setShowProjectDialog(true)}><FolderOpen size={15}/> Start with a file from this computer</button>
        <ol className="how-it-works" aria-label="How it works"><li><b>Get the video</b><span>Downloads the link or copies your file.</span></li><li><b>Find the moments</b><span>Ranks talk by hook, pace and energy, or gameplay by game sound and on-screen action.</span></li><li><b>Render shorts</b><span>9:16 video framed on the faces, with title, description and hashtags to copy.</span></li></ol>
      </form>
      <section className="home-list" aria-label="Projects">
        <div className="list-head">
          <h2>Projects</h2><span className="muted-tag">{projects.length}</span>
          <input className="project-search" type="search" aria-label="Search projects" placeholder="Search projects" value={projectSearch} onChange={event=>setProjectSearch(event.target.value)}/>
          <select className="project-sort" aria-label="Sort projects" value={projectSort} onChange={event=>setProjectSort(event.target.value as "recent"|"name"|"shorts")}><option value="recent">Recent</option><option value="name">Name</option><option value="shorts">Most shorts</option></select>
          {selectionBar("projects",projects.filter(project=>project.id!==runningProjectId).map(project=>project.id))}
        </div>
        {(()=>{
          const query=projectSearch.trim().toLowerCase();
          const shown=projects.filter(project=>project.name.toLowerCase().includes(query)).sort((x,y)=>projectSort==="name"?x.name.localeCompare(y.name):projectSort==="shorts"?(y.shorts_ready??0)-(x.shorts_ready??0):y.created_at.localeCompare(x.created_at));
          if(projects.length===0)return <div className="list-empty">Your projects will appear here. Paste a link to start the first one.</div>;
          if(shown.length===0)return <div className="list-empty">No project matches “{projectSearch}”.</div>;
          return <div className="project-rows" role="list">{shown.map((project,index)=>{
            const running=runningProjectId===project.id;
            const selected=selecting==="projects"&&selectedIds.has(project.id);
            const statusText=running?"Making shorts…":project.shorts_ready?`${project.shorts_ready} ${project.shorts_ready===1?"short":"shorts"} ready`:project.source_count?"No shorts yet":"No video yet";
            return <div role="listitem" className={`project-row ${active?.id===project.id?"current":""} ${selected?"selected":""}`} key={project.id} style={{"--i":Math.min(index,12)} as React.CSSProperties}>
              <button className="project-open" onClick={()=>{if(selecting==="projects"){if(!running)toggleSelected(project.id);return;}setActive(project);setMainPage("studio")}} aria-label={selecting==="projects"?`Select ${project.name}`:`Open ${project.name}`} aria-pressed={selecting==="projects"?selected:undefined}>
                {selecting==="projects"&&<span className={`select-check ${selected?"on":""} ${running?"off":""}`} aria-hidden="true"/>}
                <span className="project-thumb">{projectCovers[project.id]?<img src={projectCovers[project.id]} alt=""/>:<Film size={16}/>}{running&&<span className="meter indeterminate" aria-hidden="true"><i/></span>}</span>
                <span className="project-name"><b>{project.name}</b><small>{project.source_count??0} {(project.source_count??0)===1?"video":"videos"}</small></span>
                <span className={`project-status ${running?"running":project.shorts_ready?"ready":""}`}>{statusText}</span>
                <span className="project-date num">{new Date(project.created_at).toLocaleDateString(undefined,{month:"short",day:"numeric",year:"numeric"})}</span>
              </button>
              <button className="icon-btn project-delete" aria-label={`Delete ${project.name}`} title="Delete project" onClick={()=>void deleteProject(project)} disabled={running||selecting==="projects"}><Trash2 size={15}/></button>
            </div>})}</div>;
        })()}
      </section>
    </main>:mode==="auto"?<main className="auto">
      <aside className="auto-source" aria-label="Source video">
        <div className="panel-head"><h2>Source</h2><button className="small-btn" onClick={()=>void chooseVideo()} disabled={busy||highlightScanning}><Plus size={14}/> Import file</button></div>
        <form className="link-import" onSubmit={event=>{event.preventDefault();if(active&&linkUrl.trim())void importLink(active.id,linkUrl.trim())}}><input type="url" aria-label="Video link" placeholder="Paste a YouTube link" value={linkUrl} onChange={event=>setLinkUrl(event.target.value)}/><button type="submit" className="small-btn" disabled={busy||highlightScanning||!linkUrl.trim()}>Add</button></form>
        {media&&!media.missing?<div className="auto-source-media"><video ref={sourceVideoRef} key={media.path} src={mediaSource(media.path)} controls playsInline preload="metadata" onLoadedMetadata={event=>applyPendingSeek(event.currentTarget)}/><div className="auto-source-row"><div><b>{media.file_name}</b><small>{formatDuration(media.duration_seconds)} · {media.width}×{media.height}{media.has_audio?"":" · no audio"}</small></div><button className="icon-btn" aria-label={`Remove ${media.file_name}`} title="Remove video" onClick={()=>void deleteAsset(media)} disabled={busy||highlightScanning}><Trash2 size={15}/></button></div></div>:phase==="fetch"?<div className="source-placeholder" aria-hidden="true"/>:<button className="dropzone" onClick={()=>void chooseVideo()}><div className="upload-orbit"><FolderOpen size={20}/></div><b>Add a long video</b><span>Paste a link above or pick a file.</span></button>}
        {assets.length>1&&<div className="source-list" role="list" aria-label="Videos in this project">{assets.map(item=><div role="listitem" key={item.path} className={`source-row ${media?.path===item.path?"current":""}`}><button className="source-pick" onClick={()=>setMedia(item)} disabled={highlightScanning} aria-current={media?.path===item.path?"true":undefined}><span className="source-thumb">{thumbnailUrls[item.path]?<img src={thumbnailUrls[item.path]} alt=""/>:<Film size={14}/>}</span><span className="source-name"><b>{item.file_name}</b><small>{formatDuration(item.duration_seconds)}</small></span></button><button className="icon-btn" aria-label={`Remove ${item.file_name}`} title="Remove video" onClick={()=>void deleteAsset(item)} disabled={busy||highlightScanning}><Trash2 size={14}/></button></div>)}</div>}
        <div className="auto-run">
          <label className="auto-count">Footage<select value={clipMode} onChange={event=>setClipMode(event.target.value as ClipMode)} disabled={highlightScanning}><option value="auto">Auto-detect</option><option value="speech">Talking / commentary</option><option value="gameplay">Gameplay</option></select></label>
          {clipMode!=="speech"&&<label className="auto-count">Game<input type="text" maxLength={40} placeholder="e.g. GTA 6" value={gameName} onChange={event=>setGameName(event.target.value)} disabled={highlightScanning}/></label>}
          <div className="auto-count"><span>Clip length</span><div className="segmented">{(Object.keys(CLIP_LENGTHS) as ClipLengthKey[]).map(key=><button key={key} className={clipLength===key?"current":""} onClick={()=>setClipLength(key)} disabled={highlightScanning}>{CLIP_LENGTHS[key][0]}–{CLIP_LENGTHS[key][1]}s</button>)}</div></div>
          {clipMode!=="gameplay"&&<label className="auto-count">Spoken language<select value={spokenLanguage} onChange={event=>setSpokenLanguage(event.target.value)} disabled={highlightScanning}><option value="">Auto-detect</option>{[["ml","Malayalam"],["ta","Tamil"],["hi","Hindi"],["te","Telugu"],["kn","Kannada"],["bn","Bengali"],["mr","Marathi"],["en","English"],["es","Spanish"],["ar","Arabic"]].map(([code,name])=><option key={code} value={code}>{name}</option>)}</select></label>}
          <div className="auto-count"><span>Shorts to make</span><div className="segmented">{[3,5,8].map(count=><button key={count} className={highlightCount===count?"current":""} onClick={()=>setHighlightCount(count)}>{count}</button>)}</div></div>
          {backgroundJob&&!highlightScanning&&phase==="idle"&&<p className="background-job">Another video is still being processed on this computer. Its shorts are saved when it finishes.</p>}
          {highlightScanning||phase==="fetch"||backgroundJob?<button className="stop-btn auto-go" onClick={()=>void stopAuto()}><span className="stop-square" aria-hidden="true"/> Stop</button>:<button className="primary-btn auto-go" onClick={()=>{if(active&&media)void runAuto(active.id,media)}} disabled={!active||!media||media.missing||busy}>{highlights.some(item=>item.output_path)?"Find more shorts":"Make shorts"}</button>}
        </div>
      </aside>
      <section className="auto-results" aria-label="Shorts">
        <div className="section-head"><h2>Shorts</h2>{runStartedAt!==null&&phase!=="idle"&&<span className="run-clock num">Running {formatDuration(Math.max(0,(clock-runStartedAt)/1000))}</span>}{highlights.length>0&&<span className="muted-tag">{highlights.filter(item=>item.output_path).length} of {highlights.length} ready</span>}{!highlightScanning&&selectionBar("shorts",highlights.map(item=>item.id))}</div>
        {pipeline}{highlights.length===0&&phase!=="idle"?skeletonCards:highlights.length===0?<div className="auto-empty"><b>Your shorts will appear here</b><span>Add a video, then press Make shorts. The best 15–60 second moments are cut and rendered as 9:16 videos.</span></div>:<div className="short-grid" style={{"--n":Math.min(Math.max(highlights.length,1),6)} as React.CSSProperties}>{highlights.map((item,index)=><article className={`short-card ${selecting==="shorts"&&selectedIds.has(item.id)?"selected":""}`} key={item.id} style={{"--i":index} as React.CSSProperties}>
          <div className={`short-frame ${renderingClipId===item.id?"is-rendering":""}`}>{item.output_path?<video key={item.output_path} className="short-ready" src={mediaSource(item.output_path)} controls playsInline preload="metadata"/>:media?<video src={`${mediaSource(media.path)}#t=${item.start_seconds},${item.end_seconds}`} controls playsInline muted preload="metadata"/>:null}
            {renderingClipId===item.id&&<span className="scanline" aria-hidden="true"/>}
            {!item.output_path&&<span className="short-badge">{renderingClipId===item.id?"Rendering…":phase==="render"?"Queued":"Not rendered"}</span>}
            <div className="short-top">
              {selecting==="shorts"?<label className="select-check"><input type="checkbox" checked={selectedIds.has(item.id)} onChange={()=>toggleSelected(item.id)} aria-label={`Select ${item.title}`}/></label>:<span className="short-rank">#{index+1}</span>}
              {selecting!=="shorts"&&<div className="short-icons">{item.hashtags&&<button className="overlay-btn" onClick={()=>void copyPost(item)} aria-label="Copy title, description and hashtags" title="Copy post text"><Copy size={15}/></button>}{item.output_path&&<button className="overlay-btn" onClick={()=>{if(active)void renderClip(active.id,item)}} disabled={!!renderingClipId||highlightScanning} aria-label={`Render ${item.title} again with face framing`} title="Render again (face framing)"><RefreshCw size={15}/></button>}<button className="overlay-btn" onClick={()=>setEditingPost({id:item.id,title:item.title,description:item.description,hashtags:item.hashtags})} aria-label={`Edit title, description and hashtags of ${item.title}`} title="Edit post text"><Type size={15}/></button><button className="overlay-btn" onClick={()=>editInManual(item)} aria-label={`Edit ${item.title} in Manual`} title="Edit in Manual"><Pencil size={15}/></button><button className="overlay-btn danger" onClick={()=>void deleteShort(item)} disabled={renderingClipId===item.id} aria-label={`Delete ${item.title}`} title="Delete short"><Trash2 size={15}/></button></div>}
              {item.language&&<span className="short-lang">{languageName(item.language)}</span>}
            </div>
          </div>
          <div className="short-meta"><b title={item.reason}>{item.title||"Untitled clip"}</b><small>{formatDuration(item.start_seconds)} – {formatDuration(item.end_seconds)} · {Math.round(item.end_seconds-item.start_seconds)} s · score {Math.round(item.score*100)}%</small>{item.hashtags&&<p className="short-tags">{item.hashtags}</p>}</div>
          <div className="short-actions">{item.output_path?<a className="primary-btn" href={isTauri()?mediaSource(item.output_path):`${mediaSource(item.output_path)}&download=1`} download><Download size={15}/> Download</a>:<button className="small-btn" onClick={()=>{if(active)void renderClip(active.id,item)}} disabled={!!renderingClipId||!active}>Render</button>}</div>
        </article>)}</div>}
      </section>
    </main>:<main className="studio">
      <aside className="library" aria-label="Media">
        <div className="panel-head"><h2>Media</h2><button className="small-btn" onClick={()=>void chooseVideo()} disabled={busy}><Plus size={14}/> Import</button></div>
        <form className="link-import" onSubmit={event=>{event.preventDefault();if(active&&linkUrl.trim())void importLink(active.id,linkUrl.trim())}}><input type="url" aria-label="Video link" placeholder="Paste a YouTube link" value={linkUrl} onChange={event=>setLinkUrl(event.target.value)}/><button type="submit" className="small-btn" disabled={busy||!linkUrl.trim()}>Add</button></form>
        {assets.length===0?<button className="dropzone" onClick={()=>void chooseVideo()}><div className="upload-orbit"><FolderOpen size={20}/></div><b>Add footage</b><span>Videos stay on this computer.</span></button>:<div className="asset-list">{assets.map(item=><div key={item.path} className={`asset-row ${media?.path===item.path?"asset-selected":""}`} onClick={()=>{setMedia(item);setMonitorMode("source")}}><div className="asset-thumbnail">{thumbnailUrls[item.path]?<img src={thumbnailUrls[item.path]} alt=""/>:<Film size={17}/>}</div><div className="asset-main"><b>{item.file_name}</b><small>{formatDuration(item.duration_seconds)} · {item.width}×{item.height} · {item.has_audio?"audio":"no audio"}</small></div><button className="asset-delete" aria-label={`Remove ${item.file_name} from project`} title="Remove from project" onClick={event=>{event.stopPropagation();void deleteAsset(item)}} disabled={busy}><Trash2 size={14}/></button><select aria-label={`Category for ${item.file_name}`} value={item.category} onClick={e=>e.stopPropagation()} onChange={e=>void changeCategory(item,e.target.value)}>{["Uncategorized","Footage","B-roll","Interview","Product","Archive"].map(category=><option key={category}>{category}</option>)}</select>{item.missing&&<button className="relink-btn" onClick={e=>{e.stopPropagation();void relinkAsset(item)}} disabled={busy}>Relink</button>}</div>)}</div>}
      </aside>
      <section className="stage" aria-label="Monitor and timeline">
        <div className="monitor-card">
          <div className="monitor-title"><div className="segmented" role="tablist" aria-label="Monitor">{(["source","program"] as const).map(mode=><button key={mode} role="tab" aria-selected={monitorMode===mode} className={monitorMode===mode?"current":""} onClick={()=>setMonitorMode(mode)}>{mode==="source"?"Source":"Program"}</button>)}</div><span>{monitorMode==="source"?media?.file_name??"No footage":`Timeline · ${ratio}`}</span></div>
          {monitorMode==="source"?sourceView:programView}
        </div>
        {clipMap}
        <div className="timeline-card">
          <div className="timeline-toolbar"><h2>Timeline</h2><button className="small-btn" onClick={addSelectedClip} disabled={!media||media.missing||!active}>Add whole source</button><div><button className="timeline-action" onClick={undoTimeline} disabled={!undoStack.length} aria-label="Undo timeline edit">↶</button><button className="timeline-action" onClick={redoTimeline} disabled={!redoStack.length} aria-label="Redo timeline edit">↷</button></div><span className="muted-tag">Autosave {timelineSaveStatus}</span><b>{formatDuration(timelineDuration)}</b></div>
          {timeline.length===0?<div className="timeline-empty">Add clip ideas, shots or a whole source to build your short.</div>:<div className="timeline-track">{timeline.map((clip,index)=><button key={clip.id} className={`timeline-clip ${selectedClipId===clip.id?"timeline-clip-selected":""}`} onClick={()=>{selectTimelineClip(clip,index);setMonitorMode("program")}}><span>{index+1} · {clip.file_name}</span><small>{formatDuration(clipDuration(clip))} · {clip.frame_rate} fps</small></button>)}</div>}
          {selectedClip&&<div className="timeline-edit"><label>In frame<input type="number" min={0} max={selectedClip.source_out_frame-1} step={1} value={selectedClip.source_in_frame} onChange={event=>updateClipFrames(selectedClip.id,"source_in_frame",Number(event.target.value))}/></label><label>Out frame<input type="number" min={selectedClip.source_in_frame+1} max={sourceFrameCount(selectedClip)} step={1} value={selectedClip.source_out_frame} onChange={event=>updateClipFrames(selectedClip.id,"source_out_frame",Number(event.target.value))}/></label><div className="timeline-ops"><button className="quiet-btn" onClick={()=>splitClip(selectedClip.id)}><Scissors size={13}/> Split</button><button className="quiet-btn" onClick={()=>moveClip(selectedClip.id,-1)} disabled={timeline.findIndex(clip=>clip.id===selectedClip.id)===0}>Move left</button><button className="quiet-btn" onClick={()=>moveClip(selectedClip.id,1)} disabled={timeline.findIndex(clip=>clip.id===selectedClip.id)===timeline.length-1}>Move right</button><button className="relink-btn" onClick={()=>{const next=timeline.filter(clip=>clip.id!==selectedClip.id);commitTimeline(next);setSelectedClipId(next[0]?.id??null);}}>Delete</button></div></div>}
        </div>
      </section>
      <aside className="inspector" aria-label="Inspector">
        <div className="segmented inspector-tabs" role="tablist" aria-label="Inspector">{(["clips","sound","export"] as const).map(tab=><button key={tab} role="tab" aria-selected={inspectorTab===tab} className={inspectorTab===tab?"current":""} onClick={()=>setInspectorTab(tab)}>{tab==="clips"?"Clips":tab==="sound"?"Sound":"Export"}</button>)}</div>
        <div className="inspector-body">
        {inspectorTab==="clips"&&<>
          <div className="scene-card">
            <div className="scene-header"><div><b>Auto clipper</b></div><button className="small-btn" onClick={()=>void findHighlights()} disabled={!active||!media||media.missing||highlightScanning||!media.has_audio}>{highlightScanning?"Clipping…":"Find clips"}</button></div>
            <label className="scene-threshold">Clips <b>{highlightCount}</b><input type="range" min={1} max={20} step={1} value={highlightCount} onChange={event=>setHighlightCount(Number(event.target.value))}/></label>
            {!media?<div className="scene-empty">No footage selected</div>:!media.has_audio&&clipMode==="speech"?<div className="scene-empty">This source has no audio track</div>:highlights.length===0?<div className={`scene-empty ${highlightScanning?"working listen":""}`}>{highlightScanning?(clipMode==="gameplay"?"Scanning the gameplay…":"Listening to the speech…"):`Ranks ${CLIP_LENGTHS[clipLength][0]}–${CLIP_LENGTHS[clipLength][1]} s moments`}</div>:<div className="scene-ranges">{highlights.map(item=><div className="scene-range" key={item.id}><button className="scene-preview" onClick={()=>{setMonitorMode("source");previewSceneRange(item.start_seconds)}} title={item.reason}><span>{item.title||"Untitled clip"}</span><small>{formatDuration(item.start_seconds)} – {formatDuration(item.end_seconds)} · score {Math.round(item.score*100)}%</small></button><button className="quiet-btn" onClick={()=>addSceneRange(item.start_seconds,item.end_seconds)} disabled={media.missing||timeline.length>=300}>Add</button></div>)}</div>}
          </div>
          <div className="scene-card">
            <div className="scene-header"><div><b>Scene cuts</b></div><button className="small-btn" onClick={()=>void findSceneCuts()} disabled={!active||!media||media.missing||sceneScanning}>{sceneScanning?"Scanning…":"Find"}</button></div>
            <label className="scene-threshold">Sensitivity <b>{sceneThreshold.toFixed(2)}</b><input type="range" min={0.01} max={0.9} step={0.01} value={sceneThreshold} onChange={event=>setSceneThreshold(Number(event.target.value))}/></label>
            {!media?<div className="scene-empty">No footage selected</div>:sceneRanges.length===0?<div className={`scene-empty ${sceneScanning?"working sweep":""}`}>{sceneScanning?"Looking for shot changes…":sceneCuts.length===0?"No shots found yet":"No usable ranges"}</div>:<div className="scene-ranges">{sceneRanges.map((range,index)=><div className="scene-range" key={`${range.startSeconds}-${range.endSeconds}`}><button className="scene-preview" onClick={()=>{setMonitorMode("source");previewSceneRange(range.startSeconds)}}><span>Shot {index+1}</span><small>{formatDuration(range.startSeconds)} – {formatDuration(range.endSeconds)}{range.score===null?"":` · cut ${Math.round(range.score*100)}%`}</small></button><button className="quiet-btn" onClick={()=>addSceneRange(range.startSeconds,range.endSeconds)} disabled={media.missing||timeline.length>=300}>Add</button></div>)}</div>}
          </div>
          <div className="silence-card">
            <div className="scene-header"><div><b>Silence</b></div><button className="small-btn" onClick={()=>void findSilence()} disabled={!active||!media||media.missing||silenceScanning||!media.has_audio}>{silenceScanning?"Analyzing…":"Find"}</button></div>
            <div className="silence-controls"><label>Quiet below <b>{silenceNoiseDb} dB</b><input type="range" min={-60} max={-10} step={1} value={silenceNoiseDb} onChange={event=>setSilenceNoiseDb(Number(event.target.value))}/></label><label>Minimum<input type="number" min={0.1} max={30} step={0.1} value={minimumSilenceSeconds} onChange={event=>setMinimumSilenceSeconds(Number(event.target.value))}/> sec</label></div>
            {!media?<div className="scene-empty">No audio selected</div>:!media.has_audio?<div className="scene-empty">This source has no audio track</div>:silenceRanges.length===0?<div className={`scene-empty ${silenceScanning?"working sweep":""}`}>{silenceScanning?"Measuring quiet parts…":"No quiet ranges found yet"}</div>:<div className="silence-ranges">{silenceRanges.map((range,index)=>{const [n,d]=frameRateParts(media.frame_rate);const overlaps=!!selectedClip&&selectedClip.media_path===media.path&&Math.round(range.start_seconds*n/d)<selectedClip.source_out_frame&&Math.round(range.end_seconds*n/d)>selectedClip.source_in_frame;return <div className="scene-range" key={range.id}><button className="scene-preview" onClick={()=>{setMonitorMode("source");previewSceneRange(range.start_seconds)}}><span>Quiet range {index+1}</span><small>{formatDuration(range.start_seconds)} – {formatDuration(range.end_seconds)} · {formatDuration(range.end_seconds-range.start_seconds)}</small></button><button className="quiet-btn" onClick={()=>trimSilenceFromSelected(range)} disabled={!overlaps} title={overlaps?"Cut this range out of the selected timeline clip":"Select a timeline clip that contains this range"}>Cut</button></div>})}</div>}
          </div>
        </>}
        {inspectorTab==="sound"&&<div className="sound-card">
          <div className="sound-block"><div className="sound-title"><Mic2 size={16}/> Voice-over <span className="optional">Optional</span></div><textarea value={narration} onChange={e=>setNarration(e.target.value)} placeholder="Write narration"/><div className="inline-actions"><button className="small-btn" onClick={()=>void generateVoice()} disabled={busy}>Generate system voice</button><button className="quiet-btn" onClick={()=>void pickAux("voice")}>Choose file</button></div>{voice&&<small className="file-chip">{voice.split(/[\\/]/).pop()}</small>}</div>
          <div className="sound-block"><div className="sound-title"><Music2 size={16}/> Background music <span className="optional">Optional</span></div><button className="file-picker" onClick={()=>void pickAux("music")}>{music?music.split(/[\\/]/).pop():"Choose local audio file"}<FolderOpen size={15}/></button></div>
          <div className="sound-block"><div className="sound-title"><Sparkles size={16}/> Subtitles <span className="optional">SRT import</span></div><button className="file-picker" onClick={()=>void pickAux("srt")}>{srt?srt.split(/[\\/]/).pop():"Import an .srt subtitle file"}<FolderOpen size={15}/></button></div>
          <div className="sound-block audio-timeline-editor"><div className="sound-title"><Music2 size={16}/> Audio tracks <span className="optional">Autosaved</span></div><button className="small-btn" onClick={()=>void importAudioTrack()} disabled={!active||audioTimelineProjectId!==active.id||busy}>Import audio at playhead</button>{audioAssets.length>0&&<div className="audio-library">{audioAssets.map(asset=><button key={asset.id} onClick={()=>appendAudioAsset(asset)}><span>{asset.file_name}</span><small>{formatDuration(asset.duration_seconds)} · add at playhead</small></button>)}</div>}{audioTimeline.length===0?<div className="audio-empty">Narration, music or effects placed on the timeline appear here.</div>:audioTimeline.map(clip=><div key={clip.id} className={`audio-clip-editor ${selectedAudioClipId===clip.id?"audio-clip-selected":""}`} onClick={()=>setSelectedAudioClipId(clip.id)}><div className="audio-clip-heading"><b>{clip.file_name}</b><button className="relink-btn" onClick={event=>{event.stopPropagation();setAudioTimeline(items=>items.filter(item=>item.id!==clip.id));setSelectedAudioClipId(audioTimeline.find(item=>item.id!==clip.id)?.id??null);}}>Remove</button></div><div className="audio-clip-fields"><label>Track<select value={clip.track_kind} onChange={event=>updateAudioClip(clip.id,{track_kind:event.target.value as TimelineAudioClip["track_kind"]})}><option value="voice">Voice</option><option value="music">Music</option><option value="effects">Effects</option></select></label><label>Timeline start<input type="number" min={0} max={899.99} step={0.05} value={clip.timeline_start_seconds} onChange={event=>updateAudioClip(clip.id,{timeline_start_seconds:Number(event.target.value)})}/></label><label>Source in<input type="number" min={0} max={clip.source_duration_seconds-0.01} step={0.01} value={clip.source_in_seconds} onChange={event=>updateAudioClip(clip.id,{source_in_seconds:Number(event.target.value)})}/></label><label>Duration<input type="number" min={0.01} max={Math.max(0.01,clip.source_duration_seconds-clip.source_in_seconds)} step={0.01} value={clip.duration_seconds} onChange={event=>updateAudioClip(clip.id,{duration_seconds:Number(event.target.value)})}/></label><label>Gain ×{clip.gain.toFixed(2)}<input type="range" min={0} max={2} step={0.05} value={clip.gain} onChange={event=>updateAudioClip(clip.id,{gain:Number(event.target.value)})}/></label><label>Fade in<input type="number" min={0} max={clip.duration_seconds} step={0.05} value={clip.fade_in_seconds} onChange={event=>updateAudioClip(clip.id,{fade_in_seconds:Number(event.target.value)})}/></label><label>Fade out<input type="number" min={0} max={clip.duration_seconds} step={0.05} value={clip.fade_out_seconds} onChange={event=>updateAudioClip(clip.id,{fade_out_seconds:Number(event.target.value)})}/></label></div></div>)}</div>
        </div>}
        {inspectorTab==="export"&&<div className="export-card">
          <div className="export-top"><div><b>Export</b><span>MP4 · H.264 / AAC · rendered locally with FFmpeg</span></div></div>
          <label className="full-field">Format {ratioPicker}</label>
          {timeline.length===0&&<div className="field-row"><label>Start (s)<input value={start} onChange={e=>setStart(e.target.value)} inputMode="decimal"/></label><label>Length (s)<input value={duration} onChange={e=>setDuration(e.target.value)} inputMode="decimal"/></label></div>}
          <div className="spec-row"><span>Source</span><b>{timeline.length?`Timeline · ${timeline.length} ${timeline.length===1?"clip":"clips"}`:media?.file_name??"—"}</b></div>
          <div className="spec-row"><span>Length</span><b>{formatDuration(timeline.length?timelineDuration:selectedDuration)}</b></div>
          <div className="spec-row"><span>Voice · music · subtitles</span><b>{[voice?"voice":"",music?"music":"",srt?"subtitles":""].filter(Boolean).join(" · ")||"—"}</b></div>
          <button className="export-btn" onClick={()=>void exportShort()} disabled={busy||!media||!active}>{rendering?(isTauri()?`Rendering ${renderProgress}%`:"Rendering locally…"):busy?"Processing…":<><Download size={16}/> Render short</>}</button>
          {rendering&&<button className="cancel-btn" onClick={()=>void cancelRender()}>Cancel render</button>}
          {exportPath&&<div className="success-path">Saved: {exportPath}</div>}
          <button className="text-btn preview-link" onClick={()=>setMonitorMode("program")}>Preview the timeline in the monitor</button>
        </div>}
        </div>
      </aside>
    </main>}
    {toast&&!(phase!=="idle"&&!showHome&&mode==="auto"&&!toast.error)&&<div className={`toast ${toast.error?"error":""}`} key={toast.id} role={toast.error?"alert":"status"}><span className="toast-dot" aria-hidden="true"/><p>{toast.text}</p><button className="toast-close" aria-label="Dismiss" onClick={()=>setToast(null)}>×</button><span className="toast-timer" aria-hidden="true" style={{animationDuration:toast.error?"8s":"4.5s"}}/></div>}
  </div>;
}
