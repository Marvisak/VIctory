#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <wchar.h>

static void record(FILE *log, const wchar_t *what, DWORD value) {
    SYSTEMTIME now; GetLocalTime(&now);
    fwprintf(log,L"%04u-%02u-%02u %02u:%02u:%02u %ls: 0x%08lx (%lu)\n",
        now.wYear,now.wMonth,now.wDay,now.wHour,now.wMinute,now.wSecond,
        what,(unsigned long)value,(unsigned long)value);
    fflush(log);
}
static const wchar_t *meaning(DWORD code) {
    switch(code) {
    case 0x80000003u: return L"BREAKPOINT (title assertion or intentional trap)";
    case 0xc0000005u: return L"ACCESS_VIOLATION";
    case 0xc000007bu: return L"BAD_IMAGE_FORMAT";
    case 0xc0000135u: return L"DLL_NOT_FOUND";
    case 0xc0000142u: return L"DLL_INITIALIZATION_FAILED";
    default: return L"unclassified";
    }
}
int wmain(int argc,wchar_t **argv) {
    if(argc!=4 && argc!=7) {
        fwprintf(stderr,L"Usage: victory-first-run.exe ABSOLUTE_EXE_PATH LOG_DIR TIMEOUT_SECONDS [MAX_PROCESSES COMMAND_LINE WORKING_DIR]\n");
        return 2;
    }
    wchar_t exe[32768],dir[32768],report[32768],out[32768],native[32768],work[32768];
    DWORD n=GetFullPathNameW(argv[1],32768,exe,NULL);
    DWORD d=GetFullPathNameW(argv[2],32768,dir,NULL);
    if(!n||n>=32768||!d||d>=32768||GetFileAttributesW(exe)==INVALID_FILE_ATTRIBUTES||
       (GetFileAttributesW(dir)&FILE_ATTRIBUTE_DIRECTORY)==0) return 2;
    wchar_t *end=NULL;
    unsigned long seconds=wcstoul(argv[3],&end,10);
    if(!end||*end||seconds<1||seconds>900) return 2;
    unsigned long max_processes=1;
    if(argc==7) {
        max_processes=wcstoul(argv[4],&end,10);
        /* Child processes would not necessarily match the title's WFP rule. */
        if(!end||*end||max_processes!=1) return 2;
    }
    if(swprintf(report,32768,L"%ls\\first-run.log",dir)<0||
       swprintf(out,32768,L"%ls\\stdout-stderr.log",dir)<0||
       swprintf(native,32768,L"%ls\\victory-native.log",dir)<0) return 2;
    FILE *log=_wfopen(report,L"a, ccs=UTF-8");
    if(!log) return 2;
    fwprintf(log,L"executable=%ls\nlog_directory=%ls\ntimeout_seconds=%lu\nmax_processes=%lu\n",
        exe,dir,seconds,max_processes); fflush(log);
    HANDLE stream=CreateFileW(out,GENERIC_WRITE,FILE_SHARE_READ,NULL,CREATE_ALWAYS,FILE_ATTRIBUTE_NORMAL,NULL);
    if(stream==INVALID_HANDLE_VALUE) {record(log,L"stdout open failed",GetLastError());fclose(log);return 1;}
    SetHandleInformation(stream,HANDLE_FLAG_INHERIT,HANDLE_FLAG_INHERIT);
    if(!SetEnvironmentVariableW(L"VICTORY_NATIVE_LOG_PATH",native)||
       !SetEnvironmentVariableW(L"VICTORY_NATIVE_LOG_LEVEL",L"debug")||
       !SetEnvironmentVariableW(L"RUST_BACKTRACE",L"full")) {
        record(log,L"environment setup failed",GetLastError());
        CloseHandle(stream);fclose(log);return 1;
    }
    HANDLE job=CreateJobObjectW(NULL,NULL);
    if(!job){record(log,L"job creation failed",GetLastError());CloseHandle(stream);fclose(log);return 1;}
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limit={0};
    limit.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_ACTIVE_PROCESS|JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    limit.BasicLimitInformation.ActiveProcessLimit=(DWORD)max_processes;
    if(!SetInformationJobObject(job,JobObjectExtendedLimitInformation,&limit,sizeof(limit))) {
        record(log,L"job limit failed",GetLastError());CloseHandle(job);CloseHandle(stream);fclose(log);return 1;
    }
    if(argc==7) {
        n=GetFullPathNameW(argv[6],32768,work,NULL);
        if(!n||n>=32768||(GetFileAttributesW(work)&FILE_ATTRIBUTE_DIRECTORY)==0) {
            record(log,L"invalid working directory",ERROR_BAD_PATHNAME);
            CloseHandle(job);CloseHandle(stream);fclose(log);return 2;
        }
    } else {
        wcscpy(work,exe);
        wchar_t *slash=wcsrchr(work,L'\\');
        if(!slash){record(log,L"invalid executable path",ERROR_BAD_PATHNAME);CloseHandle(job);CloseHandle(stream);fclose(log);return 2;}
        *slash=0;
    }
    STARTUPINFOW si={0}; si.cb=sizeof(si); si.dwFlags=STARTF_USESTDHANDLES;
    si.hStdInput=GetStdHandle(STD_INPUT_HANDLE);si.hStdOutput=stream;si.hStdError=stream;
    PROCESS_INFORMATION pi={0};
    wchar_t *command_line=argc==7?_wcsdup(argv[5]):NULL;
    if(argc==7&&!command_line){record(log,L"command allocation failed",ERROR_OUTOFMEMORY);CloseHandle(job);CloseHandle(stream);fclose(log);return 1;}
    BOOL created=CreateProcessW(exe,command_line,NULL,NULL,TRUE,CREATE_SUSPENDED,NULL,work,&si,&pi);
    free(command_line);
    if(!created){record(log,L"process creation failed",GetLastError());CloseHandle(job);CloseHandle(stream);fclose(log);return 1;}
    record(log,L"process created suspended, pid",pi.dwProcessId);
    if(!AssignProcessToJobObject(job,pi.hProcess)) {
        record(log,L"job assignment failed; process never resumed",GetLastError());
        TerminateProcess(pi.hProcess,1);CloseHandle(pi.hThread);CloseHandle(pi.hProcess);
        CloseHandle(job);CloseHandle(stream);fclose(log);return 1;
    }
    if(ResumeThread(pi.hThread)==(DWORD)-1) {
        record(log,L"thread resume failed",GetLastError());TerminateJobObject(job,1);
        CloseHandle(pi.hThread);CloseHandle(pi.hProcess);CloseHandle(job);CloseHandle(stream);fclose(log);return 1;
    }
    record(log,L"process resumed under limited job",0);
    DWORD waited=WaitForSingleObject(pi.hProcess,seconds*1000);
    int result=0;
    if(waited==WAIT_TIMEOUT) {
        record(log,L"timeout; terminating job",seconds);TerminateJobObject(job,1);
        WaitForSingleObject(pi.hProcess,5000);result=124;
    } else if(waited==WAIT_OBJECT_0) {
        DWORD code=0;
        if(GetExitCodeProcess(pi.hProcess,&code)) {
            record(log,L"process exit code",code);
            fwprintf(log,L"exit_code_meaning=%ls\n",meaning(code));fflush(log);
            if(code) result=1;
        } else {record(log,L"exit query failed",GetLastError());result=1;}
    } else {record(log,L"process wait failed",GetLastError());result=1;}
    CloseHandle(pi.hThread);CloseHandle(pi.hProcess);CloseHandle(job);CloseHandle(stream);
    record(log,GetFileAttributesW(native)==INVALID_FILE_ATTRIBUTES?
        L"native runtime log absent; no shim call observed":L"native runtime log present",0);
    fclose(log);return result;
}
