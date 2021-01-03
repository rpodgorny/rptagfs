import os
import sys
import errno
#from stat import *
import fcntl
# pull in some spaghetti to make this stuff work without fuse-py being installed
try:
    import _find_fuse_parts
except ImportError:
    pass
import fuse


#ROOT = '/home/radek/tmp/Tagsistant'
#ROOT = '/home/radek/tmp/scan_copy'
#ROOT = '/home/radek/tmp/xxx'


# rename situations:
# /tag1/tag2/file1.txt -> /tag1/tag2/file2.txt - rename file1.txt
# /tag1/tag2/file1.txt -> /tag2/tag1/file2.txt - rename file1.txt
# /tag1/tag2/file1.txt -> /tag1/file1.txt - remove tag2 from file1.txt
# /tag1/file1.txt -> /tag1/tag3/file1.txt - add tag3 to file1.txt
# /tag1/tag2/file1.txt -> /tag2/tag1/file1.txt - do nothing
# /tag1/tag2/file1.txt -> /tag1/tag3/file1.txt - remove tag2, add tag3 to file1.txt
# /tag1/tag2/file1.txt -> /tag1/tag3/file2.txt - remove tag2, add tag3 from file1.txt and rename to file2.txt
# /tag1/tag2 -> /tag1/tag3 - rename tag2 to tag3
# /tag1/tag2 -> /tag3/tag2 - rename tag1 to tag3? (or maybe we should disallow?)
# /tag1/tag2/tag3 -> /tag2/tag1/tag5 - rename tag3 to tag5? (or maybe we should disallow?)
# /tag1/tag2 -> /tag1 - disallowed
# /tag1/tag2 -> /tag3 - disallowed

the_inst = None  # TODO: fucking global


def tags_to_key(tags):
    return tuple(sorted(tags))


def path_to_tags(path):
    return set(os.path.dirname(path).split('/')) - set([''])


def scantree(path):
    for entry in os.scandir(path):
        yield entry
        if entry.is_dir(follow_symlinks=False):
            yield from scantree(entry.path)


def find_free_bn(bn, existing_bns):
    if bn not in existing_bns:
        return bn
    root, ext = os.path.splitext(bn)
    i = 1
    while 1:
        bn_ = '%s.__%d%s' % (root, i, ext)
        if bn_ not in existing_bns:
            return bn_
        i += 1


def make_files(path):
    files, tagdirs, by_tags = {}, {}, {}
    for entry in scantree(path):
        pth = entry.path.replace(path, '', 1)
        if entry.is_dir():
            dirname = pth
            tags = path_to_tags(pth + '/')
            tagdirs[tags_to_key(tags)] = tagdirs.get(tags_to_key(tags), set()) | set([dirname])
            for tag in tags:
                by_tags[tag] = by_tags.get(tag, set())
        elif entry.is_file():
            tags = path_to_tags(pth)
            bn = os.path.basename(pth)
            bn = find_free_bn(bn, set(files.keys()))
            files[bn] = {'ffn': pth, 'tags': tags}
            dirname = os.path.dirname(pth)
            tagdirs[tags_to_key(tags)] = tagdirs.get(tags_to_key(tags), set()) | set([dirname])
            for tag in tags:
                by_tags[tag] = by_tags.get(tag, set()) | set([bn])
    #tagdirs[tags_to_key(set(['']))] = set(['/'])  # TODO: hack for directories with no files? fix!
    return files, tagdirs, by_tags


# TODO: this is not optimal
def get_avail_tags(tags, tagdirs):
    ret = set()
    for i in tagdirs.keys():
        if tags.issubset(set(i)):
            ret |= set(i)
    ret -= tags
    print('GET_AVAIL_TAGS', tags, tagdirs, ret)
    return ret


def rename_tagdir_path(path, tag, tag_new):
    path_new = path + '/'
    while '/%s/' % tag in path_new:
        path_new = path_new.replace('/%s/' % tag, '/%s/' % tag_new, 1)
    path_new = path_new[:-1]
    return path_new


def rename_tagdir_right(path, tag, tag_new):
    assert path.endswith('/%s' % tag)
    subpath_pre, subpath_post = (path + '/').rsplit('/%s/' % tag, 1)
    print('RENNN', '.%s/%s' % (subpath_pre, tag), '.%s/%s' % (subpath_pre, tag_new))
    os.rename("." + '%s/%s' % (subpath_pre, tag), "." + '%s/%s' % (subpath_pre, tag_new))
    print('AFTER RENN')


if not hasattr(fuse, '__version__'):
    raise RuntimeError("your fuse-py doesn't know of fuse.__version__, probably it's too old.")

fuse.fuse_python_api = (0, 2)
fuse.feature_assert('stateful_files', 'has_init')


def flag2mode(flags):
    md = {os.O_RDONLY: 'rb', os.O_WRONLY: 'wb', os.O_RDWR: 'wb+'}
    m = md[flags & (os.O_RDONLY | os.O_WRONLY | os.O_RDWR)]
    if flags | os.O_APPEND:
        m = m.replace('w', 'a', 1)
    return m


class RPTagFS(fuse.Fuse):
    def __init__(self, *args, **kw):
        fuse.Fuse.__init__(self, *args, **kw)
        # do stuff to set up your filesystem here, if you want
        #import thread
        #thread.start_new_thread(self.mythread, ())
        self.root = None

#    def mythread(self):
#        """
#        The beauty of the FUSE python implementation is that with the python interp
#        running in foreground, you can have threads
#        """
#        print "mythread: started"
#        while 1:
#            time.sleep(120)
#            print "mythread: ticking"

    # TODO: actually try to reuse as much of existing structure as possible
    def _get_dir_for_tags(self, tags):
        if tags_to_key(tags) in self.tagdirs:
            return self.tagdirs[tags_to_key(tags)].copy().pop()  # TODO: invent some better rule than this randomness
        path = '/' + '/'.join(tags)
        os.makedirs("." + path)
        #os.makedirs(self.root + path)
        self.tagdirs[tags_to_key(tags)] = set([path])  # TODO: also add intermediate paths
        return path

    def _rename_file(self, fn, fn1):
        ffn = self.files[fn].get('ffn')
        tags = self.files[fn].get('tags', set())
        ffn_new = ffn.replace('/%s' % fn, '/%s' % fn1)
        assert not os.path.exists(ffn_new)
        print('THE_RENAME2', ffn, ffn_new)
        os.rename("." + ffn, "." + ffn_new)
        self.files[fn]['ffn'] = ffn_new
        self.files[fn1] = self.files.pop(fn)
        for tag in tags:
            self.by_tags[tag] = self.by_tags.get(tag, set()) - set([fn]) | set([fn1])

    def _rename_tag(self, tag, tag1):
        print('RENAME_TAG', tag, tag1)
        assert tag != tag1
        assert tag1 not in self.by_tags  # new tag name must not exist
        paths = set()
        for k_, v in self.tagdirs.items():
            k = set(k_)
            if tag not in k:
                continue
            for pth in v:
                paths.add(pth)
        #for pth in paths.copy():
        #    p = pth
        #    while p:
        #        paths.add(p)
        #        p = pth.rsplit('/', 1)[0]
        #paths = [i for i in paths if i.endswith('/' + tag)]
        #paths = sorted(paths, key=lambda x: x.count('/'), reverse=True)
        print('soustat', paths)
        while paths:
            longest = sorted(paths, key=lambda x: x.count('/'), reverse=True)[0]
            bn = os.path.basename(longest)
            if bn == tag:
                rename_tagdir_right(longest, tag, tag1)
            paths.remove(longest)
            longest_new = longest[:-len('/%s' % bn)]
            if longest_new:
                paths.add(longest_new)
        print('postsoustat', paths)
        print('kokosojka', self.tagdirs)
        for k_, v in self.tagdirs.copy().items():
            k = set(k_)
            new_v = set()
            for pth in v:
                pth_new = rename_tagdir_path(pth, tag, tag1)
                new_v.add(pth_new)
            del self.tagdirs[k_]
            new_k = (k - set([tag])) | set([tag1])
            self.tagdirs[tags_to_key(new_k)] = new_v
        print('postkokosojka', self.tagdirs)
        self.by_tags[tag1] = self.by_tags[tag]
        del self.by_tags[tag]
        print('premrdka', self.by_tags)
        for i in self.by_tags[tag1]:
            self.files[i] = {
                'ffn': rename_tagdir_path(self.files[i].get('ffn'), tag, tag1),
                'tags': (self.files[i].get('tags', set()) - set([tag])) | set([tag1]),
            }

    def _add_remove_tags(self, fn, tags_to_add, tags_to_remove):
        ffn = self.files[fn]['ffn']
        bn = os.path.basename(ffn)
        tags = self.files[fn].get('tags', set())
        tags_new = (tags | tags_to_add) - tags_to_remove
        ffn_new = '%s/%s' % (self._get_dir_for_tags(tags_new), bn)
        assert not os.path.exists(ffn_new)
        print('THE_RENAME', ffn, ffn_new)
        os.rename("." + ffn, "." + ffn_new)
        self.files[fn]['ffn'] = ffn_new
        self.files[fn]['tags'] = tags_new
        for tag in tags_to_add:
            self.by_tags[tag] = self.by_tags.get(tag, set()) | set([fn])
        for tag in tags_to_remove:
            self.by_tags[tag] = self.by_tags.get(tag, set()) - set([fn])

    def getattr(self, path):
        print('GETATTR', path)
        bn = os.path.basename(path)
        tags = path_to_tags(path)
        if bn in self.files and tags.issubset(self.files[bn].get('tags', set())):
            ffn = self.files[bn].get('ffn')
            return os.lstat("." + ffn)
        avail_tags = get_avail_tags(tags, self.tagdirs)
        if not bn or bn in avail_tags:
        #tags_ = path_to_tags(path + '/')
        #if tags_to_key(tags_) in self.tagdirs:
        #if not bn or bn in self.by_tags:
            return os.lstat('.')
        return os.lstat('./NON_EXISTENT')

    def readlink(self, path):
        bn = os.path.basename(path)
        ffn = self.files[bn].get('ffn')
        return os.readlink("." + ffn)

    def readdir(self, path, offset):
        print('READDIR', path, offset)
        tags = path_to_tags(path + '/')
        avail_tags = get_avail_tags(tags, self.tagdirs)
        print('readdir', tags, avail_tags)
        for i in avail_tags:
            yield fuse.Direntry(i)
        # TODO: the stuff below could be united but would it be fast?
        if not tags:
            fns = self.files.keys()
        else:
            print('HUP', tags)
            fns = self.by_tags.get(tags.pop(), set()).copy()
            print('HUPF', len(fns))
            for tag in tags:
                print('SUBHUP', tag)
                fns &= self.by_tags.get(tag, set())
        for fn in fns:
            yield fuse.Direntry(fn)
        print('READDIR DONE', path, offset)

    def unlink(self, path):
        bn = os.path.basename(path)
        ffn = self.files[bn].get('ffn')
        tags = self.files[bn].get('tags')
        for tag in tags:
            self.by_tags[tag] -= set([bn])
        del self.files[bn]
        os.unlink("." + ffn)

    def rmdir(self, path):
        tags = path_to_tags(path + '/')
        for i in self.tagdirs.get(tags_to_key(tags), set()):
            os.rmdir("." + i)
        self.tagdirs.pop(tags_to_key(tags), None)
        #bn = os.path.basename(path)
        #del self.by_tags[bn]

    def symlink(self, path, path1):
        # TODO
        pass
        #os.symlink(path, "." + path1)

    def rename(self, path, path1):
        print('RENAME', path, path1)
        bn = os.path.basename(path)
        bn1 = os.path.basename(path1)
        tags = path_to_tags(path)
        tags1 = path_to_tags(path1)
        tags_to_add = tags1 - tags
        tags_to_remove = tags - tags1
        if bn in self.by_tags:
            #if tags or tags1:
            #    return -errno.EOPNOTSUPP
            assert bn1 not in self.by_tags
            self._rename_tag(bn, bn1)
            return
        if bn in self.files:
            if bn == bn1:
                self._add_remove_tags(bn, tags_to_add, tags_to_remove)
            else:
                self._rename_file(bn, bn1)
            return
        raise 'wut?'
        # TODO: think of /tag1 -> /tag2/tag3/tag1 (something like "add_tags_to_tag")
        # TODO: think of /tag2/tag3/tag1 -> /tag1 (something like "remove_tags_to_tag")
        ffn = self.files[bn].get('ffn')
        tags1 = (self.files[bn].get('tags', set()) | tags_to_add) - tags_to_remove
        if tags_to_key(tags1) in self.tagdirs:
            ffn1 = self.tagdirs[tags_to_key(tags1)].copy().pop()  # TODO: .copy().pop() - why?
        else:
            # TODO: this is not very nice - find a way to reuse as much of existing paths as possible
            ffn1 = '/' + '/'.join(tags_to_key(tags1))
            os.makedirs('.' + ffn1, exist_ok=True)
            self.tagdirs[tags_to_key(tags1)] = set([ffn1])
        ffn1 = '%s/%s' % (ffn1, bn1)
        os.rename("." + ffn, "." + ffn1)
        del self.files[bn]
        for tag in tags:
            self.by_tags[tag].remove(bn)
        self.files[bn1] = {'ffn': ffn1, 'tags': tags1}
        for tag in tags1:
            self.by_tags[tag].add(bn1)

    # TODO: this one is broken
    def rename____(self, path, path1):
        print('RENAME', path, path1)
        bn = os.path.basename(path)
        if bn in self.by_tags:  # TODO: remove this limitation in the future
            tags = path_to_tags(path)
            tags1 = path_to_tags(path1)
            if tags or tags1:
                return -errno.EOPNOTSUPP
            bn1 = os.path.basename(path1)
            if bn == bn1:
                return -errno.EOPNOTSUPP
            return self._rename_tag(bn, bn1)
        if bn in self.by_tags:  # TODO: this currently should not happen because of the test above
            raise 'unexpected shit'
            bn = bn1 = None
            tags = path_to_tags(path + '/')
            tags1 = path_to_tags(path1 + '/')
        elif bn in self.files:
            bn1 = os.path.basename(path1)
            tags = path_to_tags(path)
            tags1 = path_to_tags(path1)
        else:
            raise 'wtf?'
        tags_to_add = tags1 - tags
        tags_to_remove = tags - tags1
        print('prcicky', bn, bn1, tags_to_add, tags_to_remove)
        files_new = {}
        for k, v in self.files.items():
            print('juju', k, bn)
            if bn is not None and k != bn:
                continue
            tags_ = v.get('tags', set())
            print('jeje', tags_, tags_to_add, tags_to_remove)
            #if not ((tags_to_add and tags_to_add.issubset(tags_)) or (tags_to_remove and tags_to_remove.issubset(tags_))):
            #    continue
            if (bn == bn1) \
            and not (tags_to_remove & tags_) \
            and not (tags_to_add - tags_):
                continue
            bn_ = os.path.basename(v['ffn'])
            bn1_ = bn1 if bn1 else bn_
            if bn != bn1:
                existing_bns_ = set(self.files.keys()) - set([k])  # TODO: self.files.keys could be cached?
                k_new = find_free_bn(bn1_, existing_bns_)
            else:
                k_new = k
            tags_new = (v.get('tags', set()) | tags_to_add) - tags_to_remove
            dirname_new = self._get_dir_for_tags(tags_new)
            ffn_new = '%s/%s' % (dirname_new, bn1_)
            assert not os.path.exists(ffn_new)
            print('THE_RENAME', "." + v['ffn'], "." + ffn_new, k_new, tags_new)
            os.rename("." + v['ffn'], "." + ffn_new)
            files_new[k_new] = {'ffn': ffn_new, 'tags': tags_new}
            for tag in v.get('tags', set()):
                self.by_tags[tag] = self.by_tags.get(tag, set()) - set([k])
            for tag in tags_new:
                self.by_tags[tag] = self.by_tags.get(tag, set()) | set([k_new])
        self.files = files_new
        ##dirname_new = self._get_dir_for_tags(tags_new)
        tags_to_annihilate = set()
        for k, v in self.by_tags.items():
            if not v and k in tags_to_remove:
                tags_to_annihilate.add(k)
        print('tags_to_annihilate', tags_to_annihilate)
        dirs_to_annihilate = set()
        for k, v in self.tagdirs.copy().items():
            if set(k).intersection(tags_to_annihilate):
                dirs_to_annihilate |= v
                del self.tagdirs[k]
        for tag in tags_to_annihilate:
            del self.by_tags[tag]
        for i in sorted(dirs_to_annihilate, reverse=True):
            print('THE_RMDIR', i)
            os.rmdir("." + i)
        print('by_tags', self.by_tags)
        print('tagdirs', self.tagdirs)

    def link(self, path, path1):
        # TODO
        pass
        #os.link("." + path, "." + path1)

    def chmod(self, path, mode):
        bn = os.path.basename(path)
        ffn = self.files[bn].get('ffn')
        os.chmod("." + ffn, mode)

    def chown(self, path, user, group):
        bn = os.path.basename(path)
        ffn = self.files[bn].get('ffn')
        os.chown("." + ffn, user, group)

    def truncate(self, path, len):
        bn = os.path.basename(path)
        ffn = self.files[bn].get('ffn')
        f = open("." + ffn, "a")
        f.truncate(len)
        f.close()

    def mknod(self, path, mode, dev):
        bn = os.path.basename(path)
        ffn = self.files[bn].get('ffn')
        os.mknod("." + ffn, mode, dev)

    def mkdir(self, path, mode):
        print('MKDIR', path, mode)
        tags = path_to_tags(path + "/")
        pth = self._get_dir_for_tags(tags)

    def mkdir__(self, path, mode):
        print('MKDIR', path)
        bn = os.path.basename(path)
        tags = path_to_tags(path)
        pth = self.tagdirs[tags_to_key(tags)].copy().pop()  # TODO: i don't like this random pick
        pth = '%s/%s' % (pth, bn)
        os.mkdir("." + pth, mode)
        tags = tags | set([bn])
        self.tagdirs[tags_to_key(tags)] = set([pth])
        print('tagdirs', self.tagdirs)
        self.by_tags[bn] = self.by_tags.get(bn, set())
        print('by_tags', self.by_tags)

    def utime(self, path, times):
        bn = os.path.basename(path)
        ffn = self.files[bn].get('ffn')
        os.utime("." + ffn, times)

#    The following utimens method would do the same as the above utime method.
#    We can't make it better though as the Python stdlib doesn't know of
#    subsecond preciseness in acces/modify times.
#    def utimens(self, path, ts_acc, ts_mod):
#      os.utime("." + path, (ts_acc.tv_sec, ts_mod.tv_sec))

    def access(self, path, mode):
        if path == '/':
            return None
        bn = os.path.basename(path)
        if bn in self.by_tags:
            return None
            #return os.access('.', mode)
        if bn in self.files:
            #ffn = files[bn].get('ffn')
            return None
            #return os.access('.' + ffn, mode)
        tags = path_to_tags(path + '/')
        if tags_to_key(tags) in self.tagdirs:
            return None
        return -errno.EACCES
        #if not os.access("." + path, mode):
        #    return -errno.EACCES

#    This is how we could add stub extended attribute handlers...
#    (We can't have ones which aptly delegate requests to the underlying fs
#    because Python lacks a standard xattr interface.)
#
#    def getxattr(self, path, name, size):
#        val = name.swapcase() + '@' + path
#        if size == 0:
#            # We are asked for size of the value.
#            return len(val)
#        return val
#
#    def listxattr(self, path, size):
#        # We use the "user" namespace to please XFS utils
#        aa = ["user." + a for a in ("foo", "bar")]
#        if size == 0:
#            # We are asked for size of the attr list, ie. joint size of attrs
#            # plus null separators.
#            return len("".join(aa)) + len(aa)
#        return aa

    def statfs(self):
        """
        Should return an object with statvfs attributes (f_bsize, f_frsize...).
        Eg., the return value of os.statvfs() is such a thing (since py 2.2).
        If you are not reusing an existing statvfs object, start with
        fuse.StatVFS(), and define the attributes.

        To provide usable information (ie., you want sensible df(1)
        output, you are suggested to specify the following attributes:

            - f_bsize - preferred size of file blocks, in bytes
            - f_frsize - fundamental size of file blcoks, in bytes
                [if you have no idea, use the same as blocksize]
            - f_blocks - total number of blocks in the filesystem
            - f_bfree - number of free blocks
            - f_files - total number of file inodes
            - f_ffree - nunber of free file inodes
        """
        return os.statvfs(".")

    def fsinit(self):
        os.chdir(self.root)

    class RPTagFSFile(object):
        def __init__(self, path, flags, *mode):
            print('FILE', path, flags, mode, flag2mode(flags))
            bn = os.path.basename(path)
            if bn in the_inst.files:
                tags = path_to_tags(path)
                if not tags.issubset(the_inst.files[bn].get('tags', set())):
                    raise 'invalid tags'
                ffn = the_inst.files[bn].get('ffn')
            else:
                tags = path_to_tags(path)
                # TODO: this is not very nice - find a way to reuse as much of existing paths as possible
                #ffn = '/' + '/'.join(tags_to_key(tags))
                #os.makedirs('.' + ffn, exist_ok=True)
                #the_inst.tagdirs[tags_to_key(tags)] = set([ffn])
                #ffn = '%s/%s' % (ffn, bn)
                dir_ = the_inst._get_dir_for_tags(tags)
                ffn = '%s/%s' % (dir_, bn)
                the_inst.files[bn] = {'ffn': ffn, 'tags': tags}
                for tag in tags:
                    the_inst.by_tags[tag].add(bn)
            self.file = os.fdopen(os.open("." + ffn, flags, *mode), flag2mode(flags))
            self.fd = self.file.fileno()

        def read(self, length, offset):
            self.file.seek(offset)
            return self.file.read(length)

        def write(self, buf, offset):
            self.file.seek(offset)
            self.file.write(buf)
            return len(buf)

        def release(self, flags):
            self.file.close()

        def _fflush(self):
            if 'w' in self.file.mode or 'a' in self.file.mode:
                self.file.flush()

        def fsync(self, isfsyncfile):
            self._fflush()
            if isfsyncfile and hasattr(os, 'fdatasync'):
                os.fdatasync(self.fd)
            else:
                os.fsync(self.fd)

        def flush(self):
            self._fflush()
            # cf. xmp_flush() in fusexmp_fh.c
            os.close(os.dup(self.fd))

        def fgetattr(self):
            return os.fstat(self.fd)

        def ftruncate(self, len):
            self.file.truncate(len)

        def lock(self, cmd, owner, **kw):
            # TODO: this breaks stuff for directories containing .git viewed in mc -> solve
            return -errno.EOPNOTSUPP

            # The code here is much rather just a demonstration of the locking
            # API than something which actually was seen to be useful.

            # Advisory file locking is pretty messy in Unix, and the Python
            # interface to this doesn't make it better.
            # We can't do fcntl(2)/F_GETLK from Python in a platfrom independent
            # way. The following implementation *might* work under Linux.
            #
            # if cmd == fcntl.F_GETLK:
            #     import struct
            #     lockdata = struct.pack('hhQQi', kw['l_type'], os.SEEK_SET,
            #                            kw['l_start'], kw['l_len'], kw['l_pid'])
            #     ld2 = fcntl.fcntl(self.fd, fcntl.F_GETLK, lockdata)
            #     flockfields = ('l_type', 'l_whence', 'l_start', 'l_len', 'l_pid')
            #     uld2 = struct.unpack('hhQQi', ld2)
            #     res = {}
            #     for i in xrange(len(uld2)):
            #          res[flockfields[i]] = uld2[i]
            #     return fuse.Flock(**res)

            # Convert fcntl-ish lock parameters to Python's weird
            # lockf(3)/flock(2) medley locking API...
            op = {fcntl.F_UNLCK: fcntl.LOCK_UN,
                  fcntl.F_RDLCK: fcntl.LOCK_SH,
                  fcntl.F_WRLCK: fcntl.LOCK_EX}[kw['l_type']]
            if cmd == fcntl.F_GETLK:
                return -errno.EOPNOTSUPP
            elif cmd == fcntl.F_SETLK:
                if op != fcntl.LOCK_UN:
                    op |= fcntl.LOCK_NB
            elif cmd == fcntl.F_SETLKW:
                pass
            else:
                return -errno.EINVAL
            fcntl.lockf(self.fd, op, kw['l_start'], kw['l_len'])

    def init(self):
        print('loading files from %s...' % self.root)
        self.files, self.tagdirs, self.by_tags = make_files(self.root)
        print('files', self.files)
        print('tagdirs', self.tagdirs)
        print('by_tags', self.by_tags)
        print('...done')
        #import pprint
        #pprint.pprint(self.by_tags)
        #self.RPTagFSFile.files = self.files  # HACK
        #self.RPTagFSFile.tagdirs = self.tagdirs  # HACK
        #self.RPTagFSFile.by_tags = self.by_tags  # HACK
        self.file_class = self.RPTagFSFile
        global the_inst
        the_inst = self

    def main(self, *a, **kw):
        self.init()
        return fuse.Fuse.main(self, *a, **kw)


def main():
    usage = """
Userspace nullfs-alike: mirror the filesystem tree from some point on.

""" + fuse.Fuse.fusage
    server = RPTagFS(version="%prog " + fuse.__version__,
                     usage=usage,
                     dash_s_do='setsingle')

    server.parser.add_option(mountopt="root", metavar="PATH", default='/',
                             help="mirror filesystem from under PATH [default: %default]")
    server.parse(values=server, errex=1)
    assert server.root
    assert not server.root.endswith('/')
    try:
        if server.fuse_args.mount_expected():
            os.chdir(server.root)
    except OSError:
        print("can't enter root of underlying filesystem", file=sys.stderr)
        sys.exit(1)
    server.main()
    print('after main')


if __name__ == '__main__':
    main()
