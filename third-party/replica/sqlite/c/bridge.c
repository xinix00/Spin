/* De VFS bezit geen opslag: ieder byte gaat naar de Rust-eigenaar. SQLite's
 * eigen procesglobale runtime leeft uitsluitend tussen init en end. */
#include "bridge.h"
#include <string.h>
#include <limits.h>

typedef struct {
 sqlite3_file base;
 ReplicaStorage *storage;
 uint32_t handle;
 int lock;
} ReplicaFile;
static ReplicaStorage backend;
static sqlite3 *connection;
static int active;

static int file_close(sqlite3_file *file) {
 ReplicaFile *f=(ReplicaFile *)file;
 int rc=f->storage->close(f->storage->context,f->handle);
 f->base.pMethods=0;
 return rc;
}
static int file_read(sqlite3_file *file,void *buf,int n,sqlite3_int64 off) {
 ReplicaFile *f=(ReplicaFile *)file;
 if(off<0 || n<0)return SQLITE_IOERR_READ;
 return f->storage->read(f->storage->context,f->handle,(uint64_t)off,buf,n);
}
static int file_write(sqlite3_file *file,const void *buf,int n,sqlite3_int64 off) {
 ReplicaFile *f=(ReplicaFile *)file;
 if(off<0 || n<0)return SQLITE_IOERR_WRITE;
 return f->storage->write(f->storage->context,f->handle,(uint64_t)off,buf,n);
}
static int file_truncate(sqlite3_file *file,sqlite3_int64 size) {
 ReplicaFile *f=(ReplicaFile *)file;
 if(size<0)return SQLITE_IOERR_TRUNCATE;
 return f->storage->truncate(f->storage->context,f->handle,(uint64_t)size);
}
static int file_sync(sqlite3_file *file,int flags) {
 ReplicaFile *f=(ReplicaFile *)file;
 return f->storage->sync(f->storage->context,f->handle,flags);
}
static int file_size(sqlite3_file *file,sqlite3_int64 *size) {
 ReplicaFile *f=(ReplicaFile *)file;
 uint64_t n=0;
 int rc=f->storage->size(f->storage->context,f->handle,&n);
 if(rc==SQLITE_OK && n>INT64_MAX)return SQLITE_IOERR_FSTAT;
 *size=(sqlite3_int64)n;
 return rc;
}
/* De Rust-API leent de hele runtime aan precies één verbinding. Een tweede
 * verbinding of schrijver buiten deze eigenaar is geen ondersteund model. */
static int file_lock(sqlite3_file *file,int level) {((ReplicaFile *)file)->lock=level;return SQLITE_OK;}
static int file_unlock(sqlite3_file *file,int level) {((ReplicaFile *)file)->lock=level;return SQLITE_OK;}
static int file_reserved(sqlite3_file *file,int *locked) {*locked=((ReplicaFile *)file)->lock>=SQLITE_LOCK_RESERVED;return SQLITE_OK;}
static int file_control(sqlite3_file *file,int op,void *arg) {
 if(op==SQLITE_FCNTL_LOCKSTATE){*(int *)arg=((ReplicaFile *)file)->lock;return SQLITE_OK;}
 return SQLITE_NOTFOUND;
}
static int file_sector(sqlite3_file *file) {(void)file;return 512;}
static int file_characteristics(sqlite3_file *file) {(void)file;return 0;}
static const sqlite3_io_methods methods={
 .iVersion=1,.xClose=file_close,.xRead=file_read,.xWrite=file_write,
 .xTruncate=file_truncate,.xSync=file_sync,.xFileSize=file_size,
 .xLock=file_lock,.xUnlock=file_unlock,.xCheckReservedLock=file_reserved,
 .xFileControl=file_control,.xSectorSize=file_sector,.xDeviceCharacteristics=file_characteristics
};
static int vfs_open(sqlite3_vfs *vfs,const char *name,sqlite3_file *file,int flags,int *out) {
 ReplicaFile *f=(ReplicaFile *)file;
 (void)vfs;
 memset(f,0,sizeof(*f));
 /* Tijdelijke werkdata is door TEMP_STORE=3 in SQLite-geheugen. */
 if(!name || (flags & SQLITE_OPEN_WAL))return SQLITE_CANTOPEN;
 int rc=backend.open(backend.context,name,flags,&f->handle);
 if(rc!=SQLITE_OK)return rc;
 f->storage=&backend;
 f->base.pMethods=&methods;
 if(out)*out=flags;
 return SQLITE_OK;
}
static int vfs_delete(sqlite3_vfs *vfs,const char *name,int sync_dir) {(void)vfs;return backend.remove(backend.context,name,sync_dir);}
static int vfs_access(sqlite3_vfs *vfs,const char *name,int flags,int *found) {(void)vfs;(void)flags;return backend.exists(backend.context,name,found);}
static int vfs_path(sqlite3_vfs *vfs,const char *name,int n,char *out) {
 (void)vfs;
 size_t size=strlen(name);
 if(size>255 || size>=(size_t)n)return SQLITE_CANTOPEN;
 memcpy(out,name,size+1);
 return SQLITE_OK;
}
static int vfs_random(sqlite3_vfs *vfs,int n,char *out) {(void)vfs;return backend.random(backend.context,(unsigned char *)out,n);}
static int vfs_sleep(sqlite3_vfs *vfs,int micros) {(void)vfs;(void)micros;return 0;}
static int vfs_time64(sqlite3_vfs *vfs,sqlite3_int64 *out) {
 (void)vfs;int64_t millis=0;
 int rc=backend.time(backend.context,&millis);
 if(rc!=SQLITE_OK)return rc;
 if(millis>INT64_MAX-210866760000000LL || millis<0)return SQLITE_ERROR;
 *out=millis+210866760000000LL;return SQLITE_OK;
}
static int vfs_time(sqlite3_vfs *vfs,double *out) {
 sqlite3_int64 millis;
 int rc=vfs_time64(vfs,&millis);
 if(rc==SQLITE_OK)*out=(double)millis/86400000.0;
 return rc;
}
static sqlite3_vfs vfs={
 .iVersion=2,.szOsFile=sizeof(ReplicaFile),.mxPathname=255,.zName="replica",
 .xOpen=vfs_open,.xDelete=vfs_delete,.xAccess=vfs_access,.xFullPathname=vfs_path,
 .xRandomness=vfs_random,.xSleep=vfs_sleep,.xCurrentTime=vfs_time,.xCurrentTimeInt64=vfs_time64
};
/* Ook memdb-initialisatie zoekt de standaard-VFS tijdens initialize. */
int sqlite3_os_init(void){return sqlite3_vfs_register(&vfs,1);}
int sqlite3_os_end(void){return SQLITE_OK;}

int replica_sqlite_init(void *heap,int bytes,const ReplicaStorage *storage) {
 if(active || bytes<65536 || !heap || !storage)return SQLITE_MISUSE;
 int rc=sqlite3_config(SQLITE_CONFIG_HEAP,heap,bytes,64);
 if(rc!=SQLITE_OK)return rc;
 backend=*storage;
 rc=sqlite3_initialize();
 if(rc==SQLITE_OK)rc=sqlite3_vfs_register(&vfs,1);
 if(rc!=SQLITE_OK){sqlite3_shutdown();memset(&backend,0,sizeof(backend));return rc;}
 active=1;
 return SQLITE_OK;
}
void replica_sqlite_close(sqlite3 *db) {
 if(!db)return;
 sqlite3_stmt *stmt;
 while((stmt=sqlite3_next_stmt(db,0))!=0)sqlite3_finalize(stmt);
 sqlite3_close(db);
 if(connection==db)connection=0;
}
void replica_sqlite_end(void) {
 if(!active)return;
 replica_sqlite_close(connection);
 sqlite3_vfs_unregister(&vfs);
 sqlite3_shutdown();
 memset(&backend,0,sizeof(backend));
 active=0;
}
static int cooperate(void *unused) {(void)unused;return backend.cooperate(backend.context)!=SQLITE_OK;}
int replica_sqlite_open(const char *path,sqlite3 **db) {
 if(!active || connection)return SQLITE_MISUSE;
 int rc=sqlite3_open_v2(path,db,SQLITE_OPEN_READWRITE|SQLITE_OPEN_CREATE|SQLITE_OPEN_NOMUTEX,"replica");
 if(rc!=SQLITE_OK){if(*db)sqlite3_close(*db);*db=0;return rc;}
 connection=*db;
 sqlite3_progress_handler(*db,1000,cooperate,0);
 sqlite3_extended_result_codes(*db,1);
 sqlite3_db_config(*db,SQLITE_DBCONFIG_DEFENSIVE,1,0);
 sqlite3_db_config(*db,SQLITE_DBCONFIG_TRUSTED_SCHEMA,0,0);
 /* HopFS bewaart naamverwijdering pas na een bevestigde directorybarrière.
  * EXTRA vraagt die ook voor DELETE-journals; FULL alleen kan na power loss
  * een oud journal opnieuw zichtbaar maken en een bevestigde commit terugrollen. */
 rc=sqlite3_exec(*db,"PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA; PRAGMA temp_store=MEMORY",0,0,0);
 if(rc!=SQLITE_OK){replica_sqlite_close(*db);*db=0;}
 return rc;
}
/* TRANSIENT kopieert bindings: geen geleende Rust-buffer blijft in C hangen. */
int replica_bind_text(sqlite3_stmt *s,int i,const unsigned char *p,int n){return sqlite3_bind_text(s,i,(const char *)p,n,SQLITE_TRANSIENT);}
int replica_bind_blob(sqlite3_stmt *s,int i,const unsigned char *p,int n){return sqlite3_bind_blob(s,i,p,n,SQLITE_TRANSIENT);}
